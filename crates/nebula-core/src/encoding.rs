//! Encoding and line-ending detection.
//!
//! Nebula stores every buffer as UTF-8 internally. Files that arrive in another
//! encoding are transcoded on load and written back in their original encoding
//! on save, so opening a file never silently rewrites it.

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// The on-disk encoding of a file.
///
/// The list is deliberately short: UTF-8 with and without BOM covers the
/// overwhelming majority of source files, and UTF-16 covers Windows-authored
/// files that would otherwise be mangled. Anything else is rejected loudly at
/// load time rather than being guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Encoding {
    /// UTF-8 with no byte-order mark. The default for new files.
    #[default]
    Utf8,
    /// UTF-8 preceded by an EF BB BF byte-order mark.
    Utf8Bom,
    /// UTF-16, little-endian, with a byte-order mark.
    Utf16Le,
    /// UTF-16, big-endian, with a byte-order mark.
    Utf16Be,
}

impl Encoding {
    /// The byte-order mark this encoding writes, if any.
    pub const fn bom(&self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => &[],
            Encoding::Utf8Bom => &[0xEF, 0xBB, 0xBF],
            Encoding::Utf16Le => &[0xFF, 0xFE],
            Encoding::Utf16Be => &[0xFE, 0xFF],
        }
    }

    /// Detect the encoding of `bytes` from its byte-order mark.
    ///
    /// Absent a BOM this returns [`Encoding::Utf8`]; whether the bytes actually
    /// *are* valid UTF-8 is decided by [`Encoding::decode`].
    pub fn detect(bytes: &[u8]) -> Encoding {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            Encoding::Utf8Bom
        } else if bytes.starts_with(&[0xFF, 0xFE]) {
            Encoding::Utf16Le
        } else if bytes.starts_with(&[0xFE, 0xFF]) {
            Encoding::Utf16Be
        } else {
            Encoding::Utf8
        }
    }

    /// Decode `bytes` into a `String`, stripping any byte-order mark.
    pub fn decode(&self, bytes: &[u8]) -> Result<String, CoreError> {
        let body = bytes.strip_prefix(self.bom()).unwrap_or(bytes);
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => String::from_utf8(body.to_vec()).map_err(|e| {
                CoreError::Decode(format!("invalid UTF-8 at byte {}", e.utf8_error().valid_up_to()))
            }),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                if body.len() % 2 != 0 {
                    return Err(CoreError::Decode(
                        "UTF-16 content has an odd number of bytes".into(),
                    ));
                }
                let units: Vec<u16> = body
                    .chunks_exact(2)
                    .map(|c| match self {
                        Encoding::Utf16Le => u16::from_le_bytes([c[0], c[1]]),
                        _ => u16::from_be_bytes([c[0], c[1]]),
                    })
                    .collect();
                String::from_utf16(&units)
                    .map_err(|_| CoreError::Decode("invalid UTF-16 surrogate pair".into()))
            }
        }
    }

    /// Encode `text` back into bytes, re-emitting the byte-order mark.
    pub fn encode(&self, text: &str) -> Vec<u8> {
        let mut out = self.bom().to_vec();
        match self {
            Encoding::Utf8 | Encoding::Utf8Bom => out.extend_from_slice(text.as_bytes()),
            Encoding::Utf16Le => {
                for unit in text.encode_utf16() {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
            Encoding::Utf16Be => {
                for unit in text.encode_utf16() {
                    out.extend_from_slice(&unit.to_be_bytes());
                }
            }
        }
        out
    }
}

/// The line terminator convention a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum LineEnding {
    /// `\n` — Unix, and the default for new files on every platform.
    #[default]
    Lf,
    /// `\r\n` — Windows.
    Crlf,
}

impl LineEnding {
    /// The characters this line ending writes.
    pub const fn as_str(&self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    /// The platform-native line ending.
    pub const fn native() -> LineEnding {
        if cfg!(windows) { LineEnding::Crlf } else { LineEnding::Lf }
    }

    /// Detect the dominant line ending in `text`.
    ///
    /// A file is treated as CRLF when the majority of its terminators are CRLF,
    /// so a stray `\n` in an otherwise-Windows file does not flip the whole file
    /// on the next save.
    pub fn detect(text: &str) -> LineEnding {
        let bytes = text.as_bytes();
        let mut crlf = 0usize;
        let mut lf = 0usize;
        for idx in memchr::memchr_iter(b'\n', bytes) {
            if idx > 0 && bytes[idx - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
        if crlf > lf { LineEnding::Crlf } else { LineEnding::Lf }
    }

    /// Rewrite every terminator in `text` to this line ending.
    pub fn normalize(&self, text: &str) -> String {
        // Fast path: nothing to do for an already-LF file being normalised to LF.
        if *self == LineEnding::Lf && !text.contains('\r') {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len() + text.len() / 16);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    // Swallow the \n of a \r\n pair; a lone \r is also a terminator.
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    out.push_str(self.as_str());
                }
                '\n' => out.push_str(self.as_str()),
                other => out.push(other),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_boms() {
        assert_eq!(Encoding::detect(b"plain"), Encoding::Utf8);
        assert_eq!(Encoding::detect(&[0xEF, 0xBB, 0xBF, b'a']), Encoding::Utf8Bom);
        assert_eq!(Encoding::detect(&[0xFF, 0xFE, b'a', 0]), Encoding::Utf16Le);
        assert_eq!(Encoding::detect(&[0xFE, 0xFF, 0, b'a']), Encoding::Utf16Be);
    }

    #[test]
    fn utf8_bom_round_trips_without_leaking_into_the_text() {
        let bytes = Encoding::Utf8Bom.encode("héllo");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(Encoding::Utf8Bom.decode(&bytes).unwrap(), "héllo");
    }

    #[test]
    fn utf16_round_trips_both_endians() {
        for enc in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let bytes = enc.encode("hello 🌌 world");
            assert_eq!(Encoding::detect(&bytes), enc);
            assert_eq!(enc.decode(&bytes).unwrap(), "hello 🌌 world");
        }
    }

    #[test]
    fn odd_length_utf16_is_an_error_not_a_panic() {
        let err = Encoding::Utf16Le.decode(&[0xFF, 0xFE, 0x41]);
        assert!(matches!(err, Err(CoreError::Decode(_))));
    }

    #[test]
    fn line_ending_detection_takes_the_majority() {
        assert_eq!(LineEnding::detect("a\nb\nc\r\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\r\nc\n"), LineEnding::Crlf);
        assert_eq!(LineEnding::detect("no terminators"), LineEnding::Lf);
    }

    #[test]
    fn normalize_collapses_mixed_terminators() {
        assert_eq!(LineEnding::Lf.normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(LineEnding::Crlf.normalize("a\nb\r\nc"), "a\r\nb\r\nc");
    }
}
