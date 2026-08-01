//! Positions and ranges.
//!
//! Nebula uses **char offsets** as the canonical internal coordinate: they are
//! what `ropey` indexes natively, so conversions on the hot path are O(log n)
//! and never require scanning. Line/column pairs exist for the UI and for the
//! protocol layers (LSP speaks UTF-16 columns, tree-sitter speaks byte offsets),
//! and the conversions live in [`crate::text::TextBuffer`].

use serde::{Deserialize, Serialize};

/// A line/column position. Both fields are zero-based.
///
/// `column` counts **characters**, not bytes and not grapheme clusters. The LSP
/// layer converts to UTF-16 code units at the boundary; the renderer converts to
/// grapheme clusters for cursor movement.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Position {
    /// Zero-based line index.
    pub line: usize,
    /// Zero-based character column within the line.
    pub column: usize,
}

impl Position {
    /// The position at the very start of a document.
    pub const ZERO: Position = Position { line: 0, column: 0 };

    /// Construct a position.
    #[inline]
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

impl std::fmt::Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 1-based for humans, matching every compiler diagnostic ever printed.
        write!(f, "{}:{}", self.line + 1, self.column + 1)
    }
}

/// A half-open range of char offsets: `[start, end)`.
///
/// Ranges are always normalised so that `start <= end`; use [`Range::new`],
/// which sorts its arguments, rather than constructing the struct literally when
/// the order is not statically known.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Range {
    /// Inclusive start, in char offsets.
    pub start: usize,
    /// Exclusive end, in char offsets.
    pub end: usize,
}

impl Range {
    /// Construct a range, sorting the endpoints so the result is never inverted.
    #[inline]
    pub fn new(a: usize, b: usize) -> Self {
        if a <= b { Self { start: a, end: b } } else { Self { start: b, end: a } }
    }

    /// An empty range at `offset`.
    #[inline]
    pub const fn empty(offset: usize) -> Self {
        Self { start: offset, end: offset }
    }

    /// Number of characters covered.
    #[inline]
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the range covers no characters.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Whether `offset` falls inside the half-open range.
    #[inline]
    pub const fn contains(&self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }

    /// Whether `offset` falls inside the range or exactly on its end.
    ///
    /// Useful for cursor hit-testing, where a cursor sitting at the end of a
    /// selection is still "in" it.
    #[inline]
    pub const fn contains_inclusive(&self, offset: usize) -> bool {
        self.start <= offset && offset <= self.end
    }

    /// Whether two ranges share at least one character.
    #[inline]
    pub const fn overlaps(&self, other: &Range) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Whether two ranges overlap or merely touch end-to-start.
    #[inline]
    pub const fn touches(&self, other: &Range) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// The smallest range covering both inputs.
    #[inline]
    pub fn union(&self, other: &Range) -> Range {
        Range { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    /// The overlapping portion, if any.
    #[inline]
    pub fn intersection(&self, other: &Range) -> Option<Range> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start <= end).then_some(Range { start, end })
    }
}

impl From<std::ops::Range<usize>> for Range {
    fn from(r: std::ops::Range<usize>) -> Self {
        Range::new(r.start, r.end)
    }
}

impl From<Range> for std::ops::Range<usize> {
    fn from(r: Range) -> Self {
        r.start..r.end
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}..{})", self.start, self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_new_sorts_endpoints() {
        assert_eq!(Range::new(9, 3), Range { start: 3, end: 9 });
        assert_eq!(Range::new(3, 9), Range { start: 3, end: 9 });
    }

    #[test]
    fn overlap_is_exclusive_at_the_seam() {
        let a = Range::new(0, 5);
        let b = Range::new(5, 10);
        assert!(!a.overlaps(&b), "adjacent ranges must not count as overlapping");
        assert!(a.touches(&b), "adjacent ranges do touch");
    }

    #[test]
    fn intersection_of_disjoint_ranges_is_none() {
        assert_eq!(Range::new(0, 3).intersection(&Range::new(7, 9)), None);
        assert_eq!(Range::new(0, 8).intersection(&Range::new(4, 12)), Some(Range::new(4, 8)));
    }

    #[test]
    fn contains_is_half_open() {
        let r = Range::new(2, 5);
        assert!(r.contains(2));
        assert!(r.contains(4));
        assert!(!r.contains(5));
        assert!(r.contains_inclusive(5));
    }

    #[test]
    fn position_displays_one_based() {
        assert_eq!(Position::new(0, 0).to_string(), "1:1");
        assert_eq!(Position::new(41, 7).to_string(), "42:8");
    }
}
