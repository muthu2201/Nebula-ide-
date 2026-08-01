//! Word boundaries for cursor motion and selection.
//!
//! The classification is the one every code editor converges on: identifiers,
//! punctuation, and whitespace are three distinct classes, and a word motion
//! stops at every class transition. That is what makes `ctrl-left` stop between
//! `foo` and `(` in `foo(bar)` instead of skipping the whole call.

use crate::text::TextBuffer;

/// The character classes word motion distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    /// Spaces, tabs, newlines.
    Whitespace,
    /// Letters, digits, and `_` — the characters that make up identifiers.
    Word,
    /// Everything else: operators, brackets, quotes.
    Punctuation,
}

impl CharClass {
    /// Classify a single character.
    pub fn of(c: char) -> CharClass {
        if c.is_whitespace() {
            CharClass::Whitespace
        } else if c.is_alphanumeric() || c == '_' {
            CharClass::Word
        } else {
            CharClass::Punctuation
        }
    }
}

/// Find the start of the word-ish run at or before `offset`.
///
/// Leading whitespace is skipped first, so pressing `ctrl-left` from the start
/// of an indented line jumps to the end of the previous line's last word rather
/// than stopping once per space.
pub fn prev_word_boundary(buffer: &TextBuffer, offset: usize) -> usize {
    let offset = buffer.clamp_offset(offset);
    if offset == 0 {
        return 0;
    }
    let rope = buffer.rope();
    let mut idx = offset;

    // Step back over whitespace.
    while idx > 0 && CharClass::of(rope.char(idx - 1)) == CharClass::Whitespace {
        idx -= 1;
    }
    if idx == 0 {
        return 0;
    }
    // Then back over one run of a single class.
    let class = CharClass::of(rope.char(idx - 1));
    while idx > 0 && CharClass::of(rope.char(idx - 1)) == class {
        idx -= 1;
    }
    idx
}

/// Find the end of the word-ish run at or after `offset`.
pub fn next_word_boundary(buffer: &TextBuffer, offset: usize) -> usize {
    let len = buffer.len_chars();
    let offset = buffer.clamp_offset(offset);
    if offset >= len {
        return len;
    }
    let rope = buffer.rope();
    let mut idx = offset;

    // Move over one run of a single class...
    let class = CharClass::of(rope.char(idx));
    while idx < len && CharClass::of(rope.char(idx)) == class {
        idx += 1;
    }
    // ...then over the whitespace that follows it, so the cursor lands on the
    // next thing you can actually edit.
    if class != CharClass::Whitespace {
        while idx < len && CharClass::of(rope.char(idx)) == CharClass::Whitespace {
            idx += 1;
        }
    }
    idx
}

/// The range of the word containing `offset`, or an empty range if `offset` is
/// not inside a word.
///
/// This is what double-click selection and "expand selection to word" use, and
/// what the completion engine uses to find the prefix being typed.
pub fn word_at(buffer: &TextBuffer, offset: usize) -> crate::position::Range {
    let len = buffer.len_chars();
    let offset = buffer.clamp_offset(offset);
    let rope = buffer.rope();

    // Prefer the character before the cursor: with the caret at `foo|`, the word
    // being typed is `foo`, not whatever follows.
    let before_is_word = offset > 0 && CharClass::of(rope.char(offset - 1)) == CharClass::Word;
    let after_is_word = offset < len && CharClass::of(rope.char(offset)) == CharClass::Word;

    if !before_is_word && !after_is_word {
        return crate::position::Range::empty(offset);
    }
    let class = CharClass::Word;

    let mut start = offset;
    while start > 0 && CharClass::of(rope.char(start - 1)) == class {
        start -= 1;
    }
    let mut end = offset;
    while end < len && CharClass::of(rope.char(end)) == class {
        end += 1;
    }
    crate::position::Range { start, end }
}

/// Indentation (leading whitespace) of the line containing `offset`.
pub fn line_indent(buffer: &TextBuffer, line: usize) -> String {
    let Ok(text) = buffer.line(line) else {
        return String::new();
    };
    text.chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> TextBuffer {
        TextBuffer::from_str(s)
    }

    #[test]
    fn classes_split_identifiers_from_punctuation() {
        assert_eq!(CharClass::of('a'), CharClass::Word);
        assert_eq!(CharClass::of('_'), CharClass::Word);
        assert_eq!(CharClass::of('9'), CharClass::Word);
        assert_eq!(CharClass::of('é'), CharClass::Word);
        assert_eq!(CharClass::of('('), CharClass::Punctuation);
        assert_eq!(CharClass::of(' '), CharClass::Whitespace);
        assert_eq!(CharClass::of('\n'), CharClass::Whitespace);
    }

    #[test]
    fn word_motion_stops_at_class_transitions() {
        let buf = b("foo(bar)");
        // From 0: over `foo`, stopping at `(`.
        assert_eq!(next_word_boundary(&buf, 0), 3);
        // From 3: over `(`.
        assert_eq!(next_word_boundary(&buf, 3), 4);
        // From 4: over `bar`.
        assert_eq!(next_word_boundary(&buf, 4), 7);
    }

    #[test]
    fn forward_motion_absorbs_trailing_whitespace() {
        let buf = b("one   two");
        assert_eq!(next_word_boundary(&buf, 0), 6, "lands on `two`, not on the spaces");
    }

    #[test]
    fn backward_motion_skips_whitespace_first() {
        let buf = b("one   two");
        assert_eq!(prev_word_boundary(&buf, 9), 6);
        assert_eq!(prev_word_boundary(&buf, 6), 0, "crosses the gap in one step");
    }

    #[test]
    fn motion_is_clamped_at_the_ends() {
        let buf = b("word");
        assert_eq!(prev_word_boundary(&buf, 0), 0);
        assert_eq!(next_word_boundary(&buf, 4), 4);
        assert_eq!(next_word_boundary(&buf, 999), 4);
    }

    #[test]
    fn word_at_prefers_the_char_before_the_caret() {
        let buf = b("alpha beta");
        // Caret just after `alpha` selects `alpha`, which is what completion needs.
        assert_eq!(word_at(&buf, 5), crate::position::Range::new(0, 5));
        assert_eq!(word_at(&buf, 7), crate::position::Range::new(6, 10));
    }

    #[test]
    fn word_at_returns_empty_between_punctuation() {
        let buf = b("a + b");
        assert!(word_at(&buf, 2).is_empty());
    }

    #[test]
    fn indent_is_extracted_verbatim() {
        let buf = b("    indented\n\ttabbed\nflush");
        assert_eq!(line_indent(&buf, 0), "    ");
        assert_eq!(line_indent(&buf, 1), "\t");
        assert_eq!(line_indent(&buf, 2), "");
    }

    #[test]
    fn motion_crosses_newlines() {
        let buf = b("first\nsecond");
        assert_eq!(next_word_boundary(&buf, 0), 6, "trailing newline is whitespace");
        assert_eq!(prev_word_boundary(&buf, 6), 0);
    }
}
