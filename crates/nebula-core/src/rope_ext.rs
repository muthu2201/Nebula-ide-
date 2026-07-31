//! Helpers over `ropey` that several crates need but that do not belong on
//! [`TextBuffer`] itself.

use ropey::Rope;

/// Iterate the rope's chunks as byte slices.
///
/// tree-sitter parses from a chunk callback and regex search runs over byte
/// slices; both want the rope's internal chunks without an intermediate
/// `String` allocation.
pub fn byte_chunks(rope: &Rope) -> impl Iterator<Item = &[u8]> {
    rope.chunks().map(|c| c.as_bytes())
}

/// Return the chunk containing `byte_idx` along with the byte offset at which
/// that chunk starts.
///
/// This is exactly the shape tree-sitter's `TextProvider`/parse callback wants:
/// "give me the text at this byte offset and I will call you again for the
/// next piece".
pub fn chunk_at_byte(rope: &Rope, byte_idx: usize) -> (&str, usize) {
    if byte_idx >= rope.len_bytes() {
        return ("", rope.len_bytes());
    }
    let (chunk, chunk_byte_idx, _, _) = rope.chunk_at_byte(byte_idx);
    (chunk, chunk_byte_idx)
}

/// Count how many times `needle` occurs in the rope.
///
/// Used by the search UI for match counts, where materialising the whole
/// document as a `String` would defeat the point of using a rope.
pub fn count_occurrences(rope: &Rope, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    // Chunk boundaries can split a match, so search a sliding window that
    // carries `needle.len() - 1` bytes of overlap across the seam.
    let overlap = needle.len() - 1;
    let mut count = 0usize;
    let mut carry = String::new();

    for chunk in rope.chunks() {
        carry.push_str(chunk);
        let mut search_from = 0;
        while let Some(pos) = carry[search_from..].find(needle) {
            count += 1;
            search_from += pos + needle.len();
        }
        // Keep only what could still be the prefix of a match.
        let keep = carry.len().saturating_sub(overlap).max(search_from);
        if keep > 0 && keep <= carry.len() {
            // Do not slice through a UTF-8 boundary.
            let mut boundary = keep;
            while boundary < carry.len() && !carry.is_char_boundary(boundary) {
                boundary += 1;
            }
            carry.drain(..boundary.min(carry.len()));
        }
    }
    count
}

/// Whether the rope's content is entirely ASCII.
///
/// The renderer takes a substantially faster shaping path for ASCII-only lines,
/// so it is worth answering this question once per document rather than per
/// frame.
pub fn is_ascii(rope: &Rope) -> bool {
    rope.chunks().all(|c| c.is_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_reassemble_into_the_original_bytes() {
        let text: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let rope = Rope::from_str(&text);
        let joined: Vec<u8> = byte_chunks(&rope).flatten().copied().collect();
        assert_eq!(joined, text.as_bytes());
    }

    #[test]
    fn chunk_at_byte_reports_its_own_start() {
        let text: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let rope = Rope::from_str(&text);
        for probe in [0, 100, 5000, text.len() - 1] {
            let (chunk, start) = chunk_at_byte(&rope, probe);
            assert!(start <= probe);
            assert!(probe < start + chunk.len());
            assert_eq!(&text[start..start + chunk.len()], chunk);
        }
    }

    #[test]
    fn chunk_at_end_of_buffer_is_empty() {
        let rope = Rope::from_str("abc");
        assert_eq!(chunk_at_byte(&rope, 3), ("", 3));
    }

    #[test]
    fn occurrences_are_counted_across_chunk_boundaries() {
        // Large enough to span many internal chunks.
        let unit = "needle in a haystack of text; ";
        let text = unit.repeat(4000);
        let rope = Rope::from_str(&text);
        assert_eq!(count_occurrences(&rope, "needle"), 4000);
        assert_eq!(count_occurrences(&rope, "haystack"), 4000);
        assert_eq!(count_occurrences(&rope, "not-present"), 0);
        assert_eq!(count_occurrences(&rope, ""), 0);
    }

    #[test]
    fn ascii_detection_is_exact() {
        assert!(is_ascii(&Rope::from_str("plain ascii text")));
        assert!(!is_ascii(&Rope::from_str("contains 🌌")));
    }
}
