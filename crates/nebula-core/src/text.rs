//! The rope-backed text buffer.

use ropey::Rope;

use crate::encoding::{Encoding, LineEnding};
use crate::position::{Position, Range};
use crate::{CoreError, Result};

/// A text buffer: a rope plus the metadata needed to write it back unchanged.
///
/// All public offsets are **char** offsets. Byte offsets are exposed separately
/// (`char_to_byte` / `byte_to_char`) for tree-sitter, which indexes bytes, and
/// UTF-16 offsets for LSP, which indexes UTF-16 code units.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    rope: Rope,
    encoding: Encoding,
    line_ending: LineEnding,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBuffer {
    /// An empty buffer with platform defaults.
    pub fn new() -> Self {
        Self { rope: Rope::new(), encoding: Encoding::default(), line_ending: LineEnding::native() }
    }

    /// Build a buffer from a string, detecting its line ending.
    ///
    /// The text is normalised to LF internally regardless of what it arrives as;
    /// the detected ending is stored and re-applied on [`TextBuffer::to_bytes`].
    /// Keeping the rope pure-LF is what lets column arithmetic stay simple —
    /// there is no invisible `\r` to skip over in the middle of a line.
    pub fn from_str(text: &str) -> Self {
        let line_ending = LineEnding::detect(text);
        let normalized = if line_ending == LineEnding::Lf && !text.contains('\r') {
            std::borrow::Cow::Borrowed(text)
        } else {
            std::borrow::Cow::Owned(LineEnding::Lf.normalize(text))
        };
        Self { rope: Rope::from_str(&normalized), encoding: Encoding::default(), line_ending }
    }

    /// Decode raw file bytes into a buffer, detecting encoding and line endings.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let encoding = Encoding::detect(bytes);
        let text = encoding.decode(bytes)?;
        let mut buf = Self::from_str(&text);
        buf.encoding = encoding;
        Ok(buf)
    }

    /// Serialise back to file bytes in the buffer's original encoding and line ending.
    pub fn to_bytes(&self) -> Vec<u8> {
        let text = self.to_string();
        let text = if self.line_ending == LineEnding::Lf {
            text
        } else {
            self.line_ending.normalize(&text)
        };
        self.encoding.encode(&text)
    }

    /// Borrow the underlying rope, for callers that need `ropey`'s full API.
    #[inline]
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// The file encoding this buffer will be written back as.
    #[inline]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Override the encoding used on save.
    #[inline]
    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
    }

    /// The line ending this buffer will be written back with.
    #[inline]
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Override the line ending used on save.
    #[inline]
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    /// Number of characters in the buffer.
    #[inline]
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Number of bytes in the buffer's internal (LF, UTF-8) representation.
    #[inline]
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Number of lines. An empty buffer has one (empty) line.
    #[inline]
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Whether the buffer contains no characters.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    /// The character at `offset`, or `None` at end-of-buffer.
    #[inline]
    pub fn char_at(&self, offset: usize) -> Option<char> {
        (offset < self.len_chars()).then(|| self.rope.char(offset))
    }

    /// Extract the text covered by `range`.
    pub fn slice(&self, range: Range) -> Result<String> {
        self.check_range(range)?;
        Ok(self.rope.slice(range.start..range.end).to_string())
    }

    /// Extract the text of `line`, including its trailing newline if present.
    pub fn line(&self, line: usize) -> Result<String> {
        self.check_line(line)?;
        Ok(self.rope.line(line).to_string())
    }

    /// The text of `line` with any trailing newline removed.
    pub fn line_trimmed(&self, line: usize) -> Result<String> {
        let mut s = self.line(line)?;
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        Ok(s)
    }

    /// Number of characters on `line`, excluding the trailing newline.
    pub fn line_len(&self, line: usize) -> Result<usize> {
        self.check_line(line)?;
        let slice = self.rope.line(line);
        let mut len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        Ok(len)
    }

    /// Char offset of the first character of `line`.
    pub fn line_start(&self, line: usize) -> Result<usize> {
        self.check_line(line)?;
        Ok(self.rope.line_to_char(line))
    }

    /// Char offset just past the last non-newline character of `line`.
    pub fn line_end(&self, line: usize) -> Result<usize> {
        Ok(self.line_start(line)? + self.line_len(line)?)
    }

    /// Which line `offset` falls on.
    pub fn offset_to_line(&self, offset: usize) -> Result<usize> {
        self.check_offset(offset)?;
        Ok(self.rope.char_to_line(offset))
    }

    /// Convert a char offset into a line/column position.
    pub fn offset_to_position(&self, offset: usize) -> Result<Position> {
        self.check_offset(offset)?;
        let line = self.rope.char_to_line(offset);
        let column = offset - self.rope.line_to_char(line);
        Ok(Position { line, column })
    }

    /// Convert a line/column position into a char offset.
    ///
    /// A column past the end of its line is clamped to the line end rather than
    /// erroring: editors routinely hold a "desired column" that exceeds the
    /// current line while moving vertically through ragged text.
    pub fn position_to_offset(&self, pos: Position) -> Result<usize> {
        let start = self.line_start(pos.line)?;
        let len = self.line_len(pos.line)?;
        Ok(start + pos.column.min(len))
    }

    /// Convert a char offset to a byte offset (for tree-sitter and regex).
    pub fn char_to_byte(&self, offset: usize) -> Result<usize> {
        self.check_offset(offset)?;
        Ok(self.rope.char_to_byte(offset))
    }

    /// Convert a byte offset to a char offset.
    pub fn byte_to_char(&self, byte: usize) -> Result<usize> {
        if byte > self.len_bytes() {
            return Err(CoreError::OffsetOutOfBounds { offset: byte, len: self.len_bytes() });
        }
        Ok(self.rope.byte_to_char(byte))
    }

    /// Convert a char offset to a UTF-16 code-unit offset (for LSP).
    pub fn char_to_utf16(&self, offset: usize) -> Result<usize> {
        self.check_offset(offset)?;
        Ok(self.rope.char_to_utf16_cu(offset))
    }

    /// Convert a UTF-16 code-unit offset to a char offset (for LSP).
    pub fn utf16_to_char(&self, cu: usize) -> Result<usize> {
        let total = self.rope.len_utf16_cu();
        if cu > total {
            return Err(CoreError::OffsetOutOfBounds { offset: cu, len: total });
        }
        Ok(self.rope.utf16_cu_to_char(cu))
    }

    /// Convert an LSP-style position (UTF-16 columns) into a char offset.
    pub fn lsp_position_to_offset(&self, line: usize, utf16_column: usize) -> Result<usize> {
        let line_start = self.line_start(line)?;
        let line_start_cu = self.rope.char_to_utf16_cu(line_start);
        let line_len_cu = {
            let end = self.line_end(line)?;
            self.rope.char_to_utf16_cu(end) - line_start_cu
        };
        let cu = line_start_cu + utf16_column.min(line_len_cu);
        Ok(self.rope.utf16_cu_to_char(cu))
    }

    /// Convert a char offset into an LSP-style position (UTF-16 columns).
    pub fn offset_to_lsp_position(&self, offset: usize) -> Result<(usize, usize)> {
        self.check_offset(offset)?;
        let line = self.rope.char_to_line(offset);
        let line_start = self.rope.line_to_char(line);
        let column = self.rope.char_to_utf16_cu(offset) - self.rope.char_to_utf16_cu(line_start);
        Ok((line, column))
    }

    /// Insert `text` at `offset`.
    pub fn insert(&mut self, offset: usize, text: &str) -> Result<()> {
        self.check_offset(offset)?;
        if !text.is_empty() {
            self.rope.insert(offset, text);
        }
        Ok(())
    }

    /// Remove the characters covered by `range`.
    pub fn remove(&mut self, range: Range) -> Result<()> {
        self.check_range(range)?;
        if !range.is_empty() {
            self.rope.remove(range.start..range.end);
        }
        Ok(())
    }

    /// Replace the characters covered by `range` with `text`.
    pub fn replace(&mut self, range: Range, text: &str) -> Result<()> {
        self.remove(range)?;
        self.insert(range.start, text)
    }

    /// Iterate over the characters starting at `offset`.
    pub fn chars_at(&self, offset: usize) -> Result<impl Iterator<Item = char> + '_> {
        self.check_offset(offset)?;
        Ok(self.rope.chars_at(offset))
    }

    /// Iterate over the lines of the buffer as owned strings.
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.rope.lines().map(|l| l.to_string())
    }

    /// A cheap content hash, used to detect on-disk changes and to key caches.
    ///
    /// This is FNV-1a over the rope's chunks: fast, allocation-free, and stable
    /// across runs. It is explicitly **not** a cryptographic hash — callers that
    /// need integrity guarantees use blake3 in `nebula-pkg`.
    pub fn content_hash(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        for chunk in self.rope.chunks() {
            for byte in chunk.as_bytes() {
                hash ^= *byte as u64;
                hash = hash.wrapping_mul(PRIME);
            }
        }
        hash
    }

    /// Clamp an arbitrary offset into the valid range for this buffer.
    #[inline]
    pub fn clamp_offset(&self, offset: usize) -> usize {
        offset.min(self.len_chars())
    }

    /// Clamp an arbitrary range into the valid range for this buffer.
    #[inline]
    pub fn clamp_range(&self, range: Range) -> Range {
        Range { start: self.clamp_offset(range.start), end: self.clamp_offset(range.end) }
    }

    fn check_offset(&self, offset: usize) -> Result<()> {
        if offset > self.len_chars() {
            return Err(CoreError::OffsetOutOfBounds { offset, len: self.len_chars() });
        }
        Ok(())
    }

    fn check_range(&self, range: Range) -> Result<()> {
        if range.start > range.end {
            return Err(CoreError::InvertedRange { start: range.start, end: range.end });
        }
        self.check_offset(range.end)
    }

    fn check_line(&self, line: usize) -> Result<()> {
        if line >= self.len_lines() {
            return Err(CoreError::LineOutOfBounds { line, lines: self.len_lines() });
        }
        Ok(())
    }
}

impl std::fmt::Display for TextBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rope)
    }
}

impl From<&str> for TextBuffer {
    fn from(s: &str) -> Self {
        TextBuffer::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_is_normalised_in_but_restored_out() {
        let buf = TextBuffer::from_str("one\r\ntwo\r\nthree");
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        // Internally pure LF, so column arithmetic never trips over a \r.
        assert_eq!(buf.to_string(), "one\ntwo\nthree");
        assert_eq!(buf.line_len(0).unwrap(), 3);
        // But the bytes written back keep the file's original convention.
        assert_eq!(buf.to_bytes(), b"one\r\ntwo\r\nthree".to_vec());
    }

    #[test]
    fn positions_round_trip_through_offsets() {
        let buf = TextBuffer::from_str("hello\nworld\n!");
        for offset in 0..=buf.len_chars() {
            let pos = buf.offset_to_position(offset).unwrap();
            assert_eq!(buf.position_to_offset(pos).unwrap(), offset, "offset {offset}");
        }
    }

    #[test]
    fn multibyte_chars_keep_char_and_byte_offsets_distinct() {
        let buf = TextBuffer::from_str("aé🌌b");
        assert_eq!(buf.len_chars(), 4);
        assert_eq!(buf.len_bytes(), 1 + 2 + 4 + 1);
        assert_eq!(buf.char_to_byte(3).unwrap(), 7);
        assert_eq!(buf.byte_to_char(7).unwrap(), 3);
    }

    #[test]
    fn utf16_offsets_account_for_surrogate_pairs() {
        // 🌌 is one char but two UTF-16 code units — the exact case that
        // desynchronises an LSP client from its server if handled naively.
        let buf = TextBuffer::from_str("a🌌b");
        assert_eq!(buf.char_to_utf16(3).unwrap(), 4);
        assert_eq!(buf.utf16_to_char(4).unwrap(), 3);
        assert_eq!(buf.offset_to_lsp_position(3).unwrap(), (0, 4));
        assert_eq!(buf.lsp_position_to_offset(0, 4).unwrap(), 3);
    }

    #[test]
    fn line_helpers_exclude_the_terminator() {
        let buf = TextBuffer::from_str("abc\ndefgh\n");
        assert_eq!(buf.line_len(0).unwrap(), 3);
        assert_eq!(buf.line_len(1).unwrap(), 5);
        assert_eq!(buf.line_trimmed(1).unwrap(), "defgh");
        assert_eq!(buf.line_start(1).unwrap(), 4);
        assert_eq!(buf.line_end(1).unwrap(), 9);
    }

    #[test]
    fn column_past_line_end_clamps_instead_of_erroring() {
        let buf = TextBuffer::from_str("ab\nlonger line\n");
        let offset = buf.position_to_offset(Position::new(0, 99)).unwrap();
        assert_eq!(offset, 2, "vertical motion holds a desired column past line end");
    }

    #[test]
    fn out_of_bounds_offsets_error() {
        let buf = TextBuffer::from_str("abc");
        assert!(buf.offset_to_position(4).is_err());
        assert!(buf.slice(Range { start: 2, end: 1 }).is_err());
        assert!(buf.line(7).is_err());
    }

    #[test]
    fn edits_apply_at_char_granularity() {
        let mut buf = TextBuffer::from_str("hello world");
        buf.replace(Range::new(6, 11), "nebula").unwrap();
        assert_eq!(buf.to_string(), "hello nebula");
        buf.insert(0, ">> ").unwrap();
        assert_eq!(buf.to_string(), ">> hello nebula");
        buf.remove(Range::new(0, 3)).unwrap();
        assert_eq!(buf.to_string(), "hello nebula");
    }

    #[test]
    fn content_hash_tracks_content_not_identity() {
        let a = TextBuffer::from_str("some content");
        let b = TextBuffer::from_str("some content");
        let c = TextBuffer::from_str("other content");
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
    }
}
