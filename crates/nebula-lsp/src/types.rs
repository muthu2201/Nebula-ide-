//! LSP wire types and offset conversion.
//!
//! Only the subset Nebula uses is modelled. Serialising the full protocol would
//! be a large amount of code that no caller reads; when a new feature is needed
//! its types are added here.

use nebula_core::TextBuffer;
use serde::{Deserialize, Serialize};

use crate::Result;

/// An LSP position: zero-based line, zero-based **UTF-16 code unit** column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 code unit offset within the line.
    pub character: u32,
}

impl Position {
    /// Construct a position.
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }

    /// Convert a Nebula char offset into an LSP position.
    pub fn from_offset(buffer: &TextBuffer, offset: usize) -> Result<Position> {
        let (line, character) = buffer.offset_to_lsp_position(offset)?;
        Ok(Position { line: line as u32, character: character as u32 })
    }

    /// Convert this position into a Nebula char offset.
    pub fn to_offset(&self, buffer: &TextBuffer) -> Result<usize> {
        Ok(buffer.lsp_position_to_offset(self.line as usize, self.character as usize)?)
    }
}

/// An LSP range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Range {
    /// Start position, inclusive.
    pub start: Position,
    /// End position, exclusive.
    pub end: Position,
}

impl Range {
    /// Construct a range.
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Convert a Nebula char range into an LSP range.
    pub fn from_core(buffer: &TextBuffer, range: nebula_core::Range) -> Result<Range> {
        Ok(Range {
            start: Position::from_offset(buffer, range.start)?,
            end: Position::from_offset(buffer, range.end)?,
        })
    }

    /// Convert this range into a Nebula char range.
    pub fn to_core(&self, buffer: &TextBuffer) -> Result<nebula_core::Range> {
        Ok(nebula_core::Range::new(self.start.to_offset(buffer)?, self.end.to_offset(buffer)?))
    }
}

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum DiagnosticSeverity {
    /// An error; the code will not build.
    Error,
    /// A warning.
    Warning,
    /// Informational.
    Information,
    /// A hint, typically rendered subtly.
    Hint,
}

impl From<u8> for DiagnosticSeverity {
    fn from(value: u8) -> Self {
        match value {
            1 => DiagnosticSeverity::Error,
            2 => DiagnosticSeverity::Warning,
            3 => DiagnosticSeverity::Information,
            // The protocol defines 1-4; anything else is treated as a hint
            // rather than dropped, so an unknown severity still surfaces.
            _ => DiagnosticSeverity::Hint,
        }
    }
}

impl From<DiagnosticSeverity> for u8 {
    fn from(value: DiagnosticSeverity) -> u8 {
        match value {
            DiagnosticSeverity::Error => 1,
            DiagnosticSeverity::Warning => 2,
            DiagnosticSeverity::Information => 3,
            DiagnosticSeverity::Hint => 4,
        }
    }
}

/// A diagnostic reported by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Where the problem is.
    pub range: Range,
    /// How serious it is. Servers may omit this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<DiagnosticSeverity>,
    /// The server's error code, which may be a string or a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<serde_json::Value>,
    /// Which tool produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The message shown to the user.
    pub message: String,
}

impl Diagnostic {
    /// Severity, defaulting to error when the server omitted it.
    ///
    /// The specification says an absent severity means the client decides.
    /// Treating it as an error is the safe reading: showing a warning as an
    /// error is a nuisance, hiding an error is a broken build the user cannot
    /// see.
    pub fn severity_or_error(&self) -> DiagnosticSeverity {
        self.severity.unwrap_or(DiagnosticSeverity::Error)
    }
}

/// A location in a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    /// The file.
    pub uri: String,
    /// Where in it.
    pub range: Range,
}

impl Location {
    /// The filesystem path this location refers to, if it is a `file:` URI.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        let url = url::Url::parse(&self.uri).ok()?;
        url.to_file_path().ok()
    }
}

/// A completion candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionItem {
    /// The text shown in the list.
    pub label: String,
    /// The kind, as an LSP numeric code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    /// Extra detail shown beside the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Documentation, which may be a string or a MarkupContent object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<serde_json::Value>,
    /// The text actually inserted, when it differs from the label.
    #[serde(rename = "insertText", default, skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
    /// A key the server supplies for its own sorting.
    #[serde(rename = "sortText", default, skip_serializing_if = "Option::is_none")]
    pub sort_text: Option<String>,
    /// A precise edit, preferred over `insert_text` when present.
    #[serde(rename = "textEdit", default, skip_serializing_if = "Option::is_none")]
    pub text_edit: Option<serde_json::Value>,
}

impl CompletionItem {
    /// The text to insert when this item is accepted.
    pub fn insertion(&self) -> &str {
        self.insert_text.as_deref().unwrap_or(&self.label)
    }

    /// The key to sort by: the server's `sortText` when it supplied one.
    ///
    /// Servers use `sortText` to override alphabetical order — putting local
    /// variables above imports, for instance — and ignoring it produces a
    /// completion list that feels worse than the same server in another editor.
    pub fn sort_key(&self) -> &str {
        self.sort_text.as_deref().unwrap_or(&self.label)
    }
}

/// Convert a filesystem path into a `file:` URI.
pub fn path_to_uri(path: &std::path::Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        // `from_file_path` only fails for a relative path; falling back keeps a
        // relative path usable rather than failing the whole request.
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_convert_through_utf16_columns() {
        let buffer = TextBuffer::from_str("fn main() {\n    let s = \"🌌\";\n}\n");

        // The character after the emoji: one char, but two UTF-16 code units.
        let emoji_offset = buffer.to_string().find('🌌').unwrap();
        let offset = buffer.byte_to_char(emoji_offset).unwrap() + 1;

        let position = Position::from_offset(&buffer, offset).unwrap();
        assert_eq!(position.line, 1);
        assert_eq!(position.character, 15, "the emoji must count as two UTF-16 code units");
        assert_eq!(position.to_offset(&buffer).unwrap(), offset);
    }

    #[test]
    fn every_offset_round_trips() {
        let buffer = TextBuffer::from_str("ascii\néàü 🌌 mixed\nlast");
        for offset in 0..=buffer.len_chars() {
            let position = Position::from_offset(&buffer, offset).unwrap();
            assert_eq!(position.to_offset(&buffer).unwrap(), offset, "offset {offset}");
        }
    }

    #[test]
    fn ranges_convert_both_ways() {
        let buffer = TextBuffer::from_str("let value = 42;\n");
        let core = nebula_core::Range::new(4, 9);
        let lsp = Range::from_core(&buffer, core).unwrap();
        assert_eq!(lsp.start, Position::new(0, 4));
        assert_eq!(lsp.end, Position::new(0, 9));
        assert_eq!(lsp.to_core(&buffer).unwrap(), core);
    }

    #[test]
    fn severities_map_to_and_from_their_wire_codes() {
        assert_eq!(DiagnosticSeverity::from(1u8), DiagnosticSeverity::Error);
        assert_eq!(DiagnosticSeverity::from(2u8), DiagnosticSeverity::Warning);
        assert_eq!(DiagnosticSeverity::from(3u8), DiagnosticSeverity::Information);
        assert_eq!(DiagnosticSeverity::from(4u8), DiagnosticSeverity::Hint);
        assert_eq!(u8::from(DiagnosticSeverity::Error), 1);
    }

    #[test]
    fn an_unknown_severity_becomes_a_hint_rather_than_being_dropped() {
        assert_eq!(DiagnosticSeverity::from(99u8), DiagnosticSeverity::Hint);
    }

    #[test]
    fn diagnostics_parse_from_a_real_server_payload() {
        let diagnostic: Diagnostic = serde_json::from_value(serde_json::json!({
            "range": {
                "start": { "line": 3, "character": 8 },
                "end": { "line": 3, "character": 14 }
            },
            "severity": 1,
            "code": "E0425",
            "source": "rustc",
            "message": "cannot find value `foo` in this scope"
        }))
        .unwrap();

        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::Error));
        assert_eq!(diagnostic.source.as_deref(), Some("rustc"));
        assert_eq!(diagnostic.code.unwrap(), serde_json::json!("E0425"));
    }

    #[test]
    fn a_numeric_diagnostic_code_is_accepted() {
        // TypeScript's server sends numeric codes; Rust's sends strings.
        let diagnostic: Diagnostic = serde_json::from_value(serde_json::json!({
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "code": 2304,
            "message": "Cannot find name 'foo'."
        }))
        .unwrap();
        assert_eq!(diagnostic.code.unwrap(), serde_json::json!(2304));
    }

    #[test]
    fn a_diagnostic_without_a_severity_is_treated_as_an_error() {
        let diagnostic: Diagnostic = serde_json::from_value(serde_json::json!({
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "message": "something is wrong"
        }))
        .unwrap();
        assert_eq!(
            diagnostic.severity_or_error(),
            DiagnosticSeverity::Error,
            "hiding a possible error is worse than over-reporting one"
        );
    }

    #[test]
    fn completion_insertion_prefers_insert_text() {
        let item: CompletionItem = serde_json::from_value(serde_json::json!({
            "label": "println!(…)",
            "insertText": "println!(\"$1\")"
        }))
        .unwrap();
        assert_eq!(item.insertion(), "println!(\"$1\")");

        let plain: CompletionItem =
            serde_json::from_value(serde_json::json!({ "label": "value" })).unwrap();
        assert_eq!(plain.insertion(), "value");
    }

    #[test]
    fn completion_sorting_respects_the_servers_sort_text() {
        let items: Vec<CompletionItem> = serde_json::from_value(serde_json::json!([
            { "label": "zebra", "sortText": "0000" },
            { "label": "apple", "sortText": "9999" }
        ]))
        .unwrap();

        let mut sorted = items.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(b.sort_key()));
        assert_eq!(sorted[0].label, "zebra", "the server's ordering must win over alphabetical");
    }

    #[test]
    fn locations_resolve_to_paths() {
        let location = Location {
            uri: "file:///home/user/project/src/main.rs".to_string(),
            range: Range::default(),
        };
        assert_eq!(
            location.path().unwrap(),
            std::path::PathBuf::from("/home/user/project/src/main.rs")
        );
    }

    #[test]
    fn a_non_file_uri_yields_no_path() {
        let location = Location { uri: "untitled:Untitled-1".to_string(), range: Range::default() };
        assert_eq!(location.path(), None);
    }

    #[test]
    fn paths_convert_to_uris_and_back() {
        let path = std::path::Path::new("/tmp/some dir/file.rs");
        let uri = path_to_uri(path);
        assert!(uri.starts_with("file:///"));
        // A space must be percent-encoded, or the server rejects the URI.
        assert!(uri.contains("%20"), "{uri}");

        let location = Location { uri, range: Range::default() };
        assert_eq!(location.path().unwrap(), path);
    }
}
