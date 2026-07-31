//! LSP message framing.
//!
//! Each message is `Content-Length: <n>\r\n\r\n<n bytes of JSON>`. The header
//! block may carry other fields, and the byte count is of the **body**, not the
//! whole message.
//!
//! The decoder is incremental because the transport is a pipe: a read can return
//! half a header, three whole messages, or one byte. A decoder that assumed a
//! read equals a message would work in testing and fail under load, which is the
//! failure mode this module exists to make impossible.

use crate::{LspError, Result};

/// Largest message accepted, as a guard against a malformed or hostile length.
///
/// Real LSP messages are small; the largest legitimate ones are full-document
/// semantic token responses, which stay well under a megabyte even for very
/// large files.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Frame a JSON body for transmission.
pub fn encode_message(body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 48);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Incrementally decodes framed messages from a byte stream.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes read from the transport.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Take the next complete message, if one is buffered.
    ///
    /// Returns `Ok(None)` when more bytes are needed — that is the normal case,
    /// not an error.
    pub fn next_message(&mut self) -> Result<Option<String>> {
        // Find the end of the header block.
        let Some(header_end) = find_subsequence(&self.buffer, b"\r\n\r\n") else {
            // Guard against a peer that never sends the terminator.
            if self.buffer.len() > 64 * 1024 {
                return Err(LspError::Framing(
                    "header block exceeded 64 KiB without a terminator".to_string(),
                ));
            }
            return Ok(None);
        };

        let headers = std::str::from_utf8(&self.buffer[..header_end])
            .map_err(|_| LspError::Framing("headers were not valid UTF-8".to_string()))?;

        let mut content_length: Option<usize> = None;
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            // Header names are case-insensitive.
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                    LspError::Framing(format!("Content-Length `{}` is not a number", value.trim()))
                })?);
            }
        }

        let Some(length) = content_length else {
            return Err(LspError::Framing("message had no Content-Length header".to_string()));
        };
        if length > MAX_MESSAGE_BYTES {
            return Err(LspError::Framing(format!(
                "Content-Length {length} exceeds the {MAX_MESSAGE_BYTES} byte limit"
            )));
        }

        let body_start = header_end + 4;
        if self.buffer.len() < body_start + length {
            // The body has not fully arrived yet.
            return Ok(None);
        }

        let body = String::from_utf8(self.buffer[body_start..body_start + length].to_vec())
            .map_err(|_| LspError::Framing("message body was not valid UTF-8".to_string()))?;

        self.buffer.drain(..body_start + length);
        Ok(Some(body))
    }

    /// How many bytes are buffered but not yet consumed.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_produces_the_expected_frame() {
        let framed = encode_message(r#"{"jsonrpc":"2.0"}"#);
        let text = String::from_utf8(framed).unwrap();
        assert_eq!(text, "Content-Length: 17\r\n\r\n{\"jsonrpc\":\"2.0\"}");
    }

    #[test]
    fn content_length_counts_bytes_not_characters() {
        // A body with multi-byte characters: the header must count bytes, or the
        // peer reads the wrong amount and every subsequent message is corrupt.
        let body = r#"{"text":"héllo 🌌"}"#;
        let framed = encode_message(body);
        let header = String::from_utf8(framed[..30].to_vec()).unwrap();
        assert!(
            header.starts_with(&format!("Content-Length: {}", body.len())),
            "header was {header:?} for a {}-byte body",
            body.len()
        );
    }

    #[test]
    fn a_whole_message_decodes() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_message(r#"{"id":1}"#));
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(r#"{"id":1}"#));
        assert_eq!(decoder.next_message().unwrap(), None);
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn several_messages_in_one_read_all_decode() {
        let mut decoder = FrameDecoder::new();
        let mut bytes = encode_message(r#"{"id":1}"#);
        bytes.extend(encode_message(r#"{"id":2}"#));
        bytes.extend(encode_message(r#"{"id":3}"#));
        decoder.feed(&bytes);

        let ids: Vec<String> =
            std::iter::from_fn(|| decoder.next_message().unwrap()).collect();
        assert_eq!(ids, vec![r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#]);
    }

    #[test]
    fn a_message_split_across_reads_decodes_once_complete() {
        let framed = encode_message(r#"{"id":42,"result":"value"}"#);
        let mut decoder = FrameDecoder::new();

        // Feed one byte at a time: the pathological case a pipe can genuinely
        // produce.
        for (index, byte) in framed.iter().enumerate() {
            decoder.feed(&[*byte]);
            let message = decoder.next_message().unwrap();
            if index + 1 < framed.len() {
                assert_eq!(message, None, "decoded early at byte {index}");
            } else {
                assert_eq!(message.as_deref(), Some(r#"{"id":42,"result":"value"}"#));
            }
        }
    }

    #[test]
    fn a_header_split_mid_field_decodes() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Len");
        assert_eq!(decoder.next_message().unwrap(), None);
        decoder.feed(b"gth: 8\r\n\r\n{\"id\":1}");
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(r#"{"id":1}"#));
    }

    #[test]
    fn extra_headers_are_tolerated() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 8\r\n\r\n{\"id\":1}",
        );
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(r#"{"id":1}"#));
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"content-length: 8\r\n\r\n{\"id\":1}");
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(r#"{"id":1}"#));
    }

    #[test]
    fn a_multibyte_body_decodes_intact() {
        let body = r#"{"text":"héllo 🌌 wörld"}"#;
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_message(body));
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(body));
    }

    #[test]
    fn a_missing_content_length_is_an_error() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Type: application/json\r\n\r\n{}");
        assert!(matches!(decoder.next_message(), Err(LspError::Framing(_))));
    }

    #[test]
    fn a_non_numeric_content_length_is_an_error() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Length: banana\r\n\r\n{}");
        assert!(matches!(decoder.next_message(), Err(LspError::Framing(_))));
    }

    #[test]
    fn an_absurd_content_length_is_rejected_before_allocating() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Length: 99999999999\r\n\r\n");
        let err = decoder.next_message().unwrap_err();
        assert!(matches!(err, LspError::Framing(_)), "{err:?}");
    }

    #[test]
    fn an_endless_header_block_is_rejected() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&vec![b'x'; 65_536 + 1]);
        assert!(matches!(decoder.next_message(), Err(LspError::Framing(_))));
    }

    #[test]
    fn an_empty_body_is_valid() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Length: 0\r\n\r\n");
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(""));
    }

    #[test]
    fn encode_and_decode_round_trip_for_large_messages() {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": (0..5_000).map(|i| format!("item {i}")).collect::<Vec<_>>()
        })
        .to_string();

        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_message(&body));
        assert_eq!(decoder.next_message().unwrap().as_deref(), Some(body.as_str()));
    }
}
