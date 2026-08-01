//! The document: text, cursors, history and version, as one addressable unit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::edit::{Edit, Transaction, TransactionResult};
use crate::encoding::{Encoding, LineEnding};
use crate::history::History;
use crate::position::{Position, Range};
use crate::selection::{Selection, SelectionSet};
use crate::text::TextBuffer;
use crate::{CoreError, Result};

/// A process-unique document identifier.
///
/// Stable across renames, unlike a path, which is what the LSP and syntax layers
/// need to key their caches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DocumentId(u64);

impl DocumentId {
    /// Allocate a fresh identifier.
    pub fn next() -> DocumentId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        DocumentId(COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw numeric value, for protocol layers that need to serialise it.
    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "doc#{}", self.0)
    }
}

/// Everything about a document that is not its text or cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentMeta {
    /// Where the document lives on disk, if anywhere.
    pub path: Option<PathBuf>,
    /// Language identifier (`rust`, `python`, ...), as used by LSP and by the
    /// syntax layer to pick a grammar.
    pub language: Option<String>,
    /// Whether the file is writable. Read-only documents reject transactions
    /// rather than letting the user type into something that cannot be saved.
    pub read_only: bool,
}

impl Default for DocumentMeta {
    fn default() -> Self {
        Self { path: None, language: None, read_only: false }
    }
}

/// A document: text buffer, cursors, undo history, and a monotonic version.
///
/// The version increments on every applied transaction and is what the LSP layer
/// sends as `textDocument/didChange.version` and what the syntax layer uses to
/// decide whether its parse tree is stale.
#[derive(Debug)]
pub struct Document {
    id: DocumentId,
    buffer: TextBuffer,
    selections: SelectionSet,
    history: History,
    meta: DocumentMeta,
    version: u64,
}

impl Document {
    /// A new, empty, unsaved document.
    pub fn new() -> Self {
        Self {
            id: DocumentId::next(),
            buffer: TextBuffer::new(),
            selections: SelectionSet::default(),
            history: History::new(),
            meta: DocumentMeta::default(),
            version: 0,
        }
    }

    /// A document holding `text`, not backed by any file.
    pub fn from_str(text: &str) -> Self {
        Self { buffer: TextBuffer::from_str(text), ..Self::new() }
    }

    /// A document loaded from file bytes, with its path and language recorded.
    pub fn from_bytes(path: impl Into<PathBuf>, bytes: &[u8]) -> Result<Self> {
        let path = path.into();
        let language = detect_language(&path);
        Ok(Self {
            buffer: TextBuffer::from_bytes(bytes)?,
            meta: DocumentMeta { path: Some(path), language, read_only: false },
            ..Self::new()
        })
    }

    /// This document's stable identifier.
    #[inline]
    pub fn id(&self) -> DocumentId {
        self.id
    }

    /// The current version, incremented once per applied transaction.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The text buffer.
    #[inline]
    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    /// The current selections.
    #[inline]
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }

    /// The undo history.
    #[inline]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Document metadata.
    #[inline]
    pub fn meta(&self) -> &DocumentMeta {
        &self.meta
    }

    /// Mutable metadata, for renames and language overrides.
    #[inline]
    pub fn meta_mut(&mut self) -> &mut DocumentMeta {
        &mut self.meta
    }

    /// The file path, if the document is backed by one.
    #[inline]
    pub fn path(&self) -> Option<&Path> {
        self.meta.path.as_deref()
    }

    /// The language identifier, if known.
    #[inline]
    pub fn language(&self) -> Option<&str> {
        self.meta.language.as_deref()
    }

    /// Whether the document has unsaved changes.
    #[inline]
    pub fn is_modified(&self) -> bool {
        self.history.is_modified()
    }

    /// The whole document as a string. O(n) — not for the hot path.
    pub fn text(&self) -> String {
        self.buffer.to_string()
    }

    /// Set the cursors, clamping them into the buffer.
    ///
    /// This closes the current undo group: a deliberate cursor move means the
    /// next keystroke starts a new undo step, so undo does not jump the caret
    /// somewhere unexpected.
    pub fn set_selections(&mut self, selections: SelectionSet) {
        let mut selections = selections;
        selections.clamp(&self.buffer);
        if selections != self.selections {
            self.history.commit_group();
        }
        self.selections = selections;
    }

    /// Place a single caret at `offset`.
    pub fn set_caret(&mut self, offset: usize) {
        self.set_selections(SelectionSet::caret(self.buffer.clamp_offset(offset)));
    }

    /// Apply a transaction, recording it in history.
    ///
    /// `groupable` should be true only for plain typing; see [`History::record`].
    pub fn apply(&mut self, transaction: Transaction, groupable: bool) -> Result<TransactionResult> {
        if self.meta.read_only {
            return Err(CoreError::Decode("document is read-only".into()));
        }
        if transaction.is_empty() {
            return Ok(TransactionResult {
                inverse: Transaction::new(),
                char_delta: 0,
                changed: None,
            });
        }

        let selections_before = self.selections.clone();
        let result = transaction.apply(&mut self.buffer)?;
        let selections_after = transaction.map_selections(&selections_before);

        self.history.record(
            transaction,
            result.inverse.clone(),
            selections_before,
            selections_after.clone(),
            groupable,
        );
        self.selections = selections_after;
        self.version += 1;
        Ok(result)
    }

    /// Insert `text` at every cursor, replacing any selected text.
    ///
    /// This is the single entry point for typing, paste, and snippet insertion.
    pub fn insert_at_cursors(&mut self, text: &str, groupable: bool) -> Result<TransactionResult> {
        let edits: Vec<Edit> =
            self.selections.iter().map(|sel| Edit::replace(sel.range(), text)).collect();
        let transaction = Transaction::from_edits(edits)?;
        self.apply(transaction, groupable)
    }

    /// Delete backwards from every cursor (the backspace key).
    ///
    /// A cursor with a selection deletes the selection; a bare caret deletes one
    /// character.
    pub fn delete_backward(&mut self) -> Result<TransactionResult> {
        let edits: Vec<Edit> = self
            .selections
            .iter()
            .filter_map(|sel| {
                if !sel.is_empty() {
                    Some(Edit::delete(sel.range()))
                } else if sel.head > 0 {
                    Some(Edit::delete(Range::new(sel.head - 1, sel.head)))
                } else {
                    None
                }
            })
            .collect();
        let transaction = Transaction::from_edits(edits)?;
        self.apply(transaction, true)
    }

    /// Delete forwards from every cursor (the delete key).
    pub fn delete_forward(&mut self) -> Result<TransactionResult> {
        let len = self.buffer.len_chars();
        let edits: Vec<Edit> = self
            .selections
            .iter()
            .filter_map(|sel| {
                if !sel.is_empty() {
                    Some(Edit::delete(sel.range()))
                } else if sel.head < len {
                    Some(Edit::delete(Range::new(sel.head, sel.head + 1)))
                } else {
                    None
                }
            })
            .collect();
        let transaction = Transaction::from_edits(edits)?;
        self.apply(transaction, true)
    }

    /// Undo one step, restoring the cursors that were active before it.
    pub fn undo(&mut self) -> Result<bool> {
        let Some(entry) = self.history.undo() else {
            return Ok(false);
        };
        entry.apply_undo(&mut self.buffer)?;
        self.selections = entry.selections_before;
        self.selections.clamp(&self.buffer);
        self.version += 1;
        Ok(true)
    }

    /// Redo one step.
    pub fn redo(&mut self) -> Result<bool> {
        let Some(entry) = self.history.redo() else {
            return Ok(false);
        };
        entry.apply_redo(&mut self.buffer)?;
        self.selections = entry.selections_after;
        self.selections.clamp(&self.buffer);
        self.version += 1;
        Ok(true)
    }

    /// Serialise for writing to disk, in the document's encoding and line ending.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.buffer.to_bytes()
    }

    /// Mark the document as matching what is on disk.
    pub fn mark_saved(&mut self) {
        self.history.mark_saved();
    }

    /// Replace the entire content, e.g. after an external change on disk.
    ///
    /// This is recorded as a single non-groupable undo step so a reload can be
    /// undone if it was not what the user wanted.
    pub fn reload(&mut self, text: &str) -> Result<TransactionResult> {
        let transaction = Transaction::single(Edit::replace(
            Range::new(0, self.buffer.len_chars()),
            text,
        ));
        self.apply(transaction, false)
    }

    /// The file encoding used on save.
    pub fn encoding(&self) -> Encoding {
        self.buffer.encoding()
    }

    /// The line ending used on save.
    pub fn line_ending(&self) -> LineEnding {
        self.buffer.line_ending()
    }

    /// Convert an offset to a line/column position.
    pub fn position_at(&self, offset: usize) -> Result<Position> {
        self.buffer.offset_to_position(offset)
    }

    /// The word under the primary cursor, if any.
    pub fn word_at_cursor(&self) -> Range {
        crate::word::word_at(&self.buffer, self.selections.primary().head)
    }

    /// Add a cursor at `offset` without disturbing the existing ones.
    pub fn add_cursor(&mut self, offset: usize) {
        let offset = self.buffer.clamp_offset(offset);
        self.selections.push(Selection::caret(offset));
        self.history.commit_group();
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

/// Map a file path to a language identifier.
///
/// The identifiers match the LSP `languageId` values, since that is where they
/// are ultimately sent. Unknown extensions return `None` rather than a guess —
/// starting the wrong language server is worse than starting none.
pub fn detect_language(path: &Path) -> Option<String> {
    // Whole-filename matches first: these have no useful extension.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let by_name = match name {
            "Cargo.toml" | "Cargo.lock" => Some("toml"),
            "Dockerfile" | "Containerfile" => Some("dockerfile"),
            "Makefile" | "makefile" | "GNUmakefile" => Some("makefile"),
            "CMakeLists.txt" => Some("cmake"),
            ".gitignore" | ".dockerignore" | ".npmignore" => Some("ignore"),
            _ => None,
        };
        if let Some(lang) = by_name {
            return Some(lang.to_string());
        }
    }

    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "swift" => "swift",
        "zig" => "zig",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "ps1" => "powershell",
        "json" => "json",
        "jsonc" => "jsonc",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "wit" => "wit",
        "wat" => "wat",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "tf" => "terraform",
        "nix" => "nix",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "scala" | "sc" => "scala",
        "dart" => "dart",
        "r" => "r",
        "jl" => "julia",
        "vim" => "vim",
        "txt" => "plaintext",
        _ => return None,
    };
    Some(lang.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ids_are_unique() {
        let a = Document::new();
        let b = Document::new();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn typing_advances_every_cursor() {
        let mut doc = Document::from_str("one\ntwo\nthree");
        doc.set_selections(SelectionSet::from_iter([
            Selection::caret(0),
            Selection::caret(4),
            Selection::caret(8),
        ]));
        doc.insert_at_cursors("> ", true).unwrap();
        assert_eq!(doc.text(), "> one\n> two\n> three");
        // Every cursor sits just after the text it inserted.
        let heads: Vec<_> = doc.selections().iter().map(|s| s.head).collect();
        assert_eq!(heads, vec![2, 8, 14]);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut doc = Document::from_str("hello world");
        doc.set_selections(SelectionSet::single(Selection::new(6, 11)));
        doc.insert_at_cursors("nebula", false).unwrap();
        assert_eq!(doc.text(), "hello nebula");
        assert_eq!(doc.selections().primary().head, 12);
    }

    #[test]
    fn backspace_at_a_caret_removes_one_char() {
        let mut doc = Document::from_str("abc");
        doc.set_caret(3);
        doc.delete_backward().unwrap();
        assert_eq!(doc.text(), "ab");
        assert_eq!(doc.selections().primary().head, 2);
    }

    #[test]
    fn backspace_at_offset_zero_is_a_noop_not_an_error() {
        let mut doc = Document::from_str("abc");
        doc.set_caret(0);
        doc.delete_backward().unwrap();
        assert_eq!(doc.text(), "abc");
    }

    #[test]
    fn backspace_with_a_selection_deletes_the_selection() {
        let mut doc = Document::from_str("keep DELETE keep");
        doc.set_selections(SelectionSet::single(Selection::new(5, 12)));
        doc.delete_backward().unwrap();
        assert_eq!(doc.text(), "keep keep");
    }

    #[test]
    fn delete_forward_at_end_of_buffer_is_a_noop() {
        let mut doc = Document::from_str("abc");
        doc.set_caret(3);
        doc.delete_forward().unwrap();
        assert_eq!(doc.text(), "abc");
    }

    #[test]
    fn undo_restores_text_and_cursors_together() {
        let mut doc = Document::from_str("base");
        doc.set_caret(4);
        doc.insert_at_cursors("!!", false).unwrap();
        assert_eq!(doc.text(), "base!!");
        assert_eq!(doc.selections().primary().head, 6);

        assert!(doc.undo().unwrap());
        assert_eq!(doc.text(), "base");
        assert_eq!(doc.selections().primary().head, 4, "cursor returns to where it was");

        assert!(doc.redo().unwrap());
        assert_eq!(doc.text(), "base!!");
        assert_eq!(doc.selections().primary().head, 6);
    }

    #[test]
    fn undo_on_a_fresh_document_reports_false() {
        let mut doc = Document::new();
        assert!(!doc.undo().unwrap());
        assert!(!doc.redo().unwrap());
    }

    #[test]
    fn version_increments_once_per_applied_transaction() {
        let mut doc = Document::from_str("x");
        assert_eq!(doc.version(), 0);
        doc.set_caret(1);
        doc.insert_at_cursors("y", false).unwrap();
        assert_eq!(doc.version(), 1);
        doc.undo().unwrap();
        assert_eq!(doc.version(), 2, "undo is itself a change the LSP must hear about");
    }

    #[test]
    fn empty_transactions_do_not_bump_the_version() {
        let mut doc = Document::from_str("x");
        doc.apply(Transaction::new(), false).unwrap();
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn read_only_documents_reject_edits() {
        let mut doc = Document::from_str("locked");
        doc.meta_mut().read_only = true;
        doc.set_caret(0);
        assert!(doc.insert_at_cursors("x", false).is_err());
        assert_eq!(doc.text(), "locked");
    }

    #[test]
    fn modified_flag_clears_on_save() {
        let mut doc = Document::from_str("content");
        assert!(!doc.is_modified());
        doc.set_caret(7);
        doc.insert_at_cursors("!", false).unwrap();
        assert!(doc.is_modified());
        doc.mark_saved();
        assert!(!doc.is_modified());
    }

    #[test]
    fn reload_replaces_everything_and_is_undoable() {
        let mut doc = Document::from_str("old content");
        doc.reload("brand new content").unwrap();
        assert_eq!(doc.text(), "brand new content");
        doc.undo().unwrap();
        assert_eq!(doc.text(), "old content");
    }

    #[test]
    fn loading_from_bytes_detects_path_language_and_encoding() {
        let doc = Document::from_bytes("/tmp/example.rs", b"fn main() {}\n").unwrap();
        assert_eq!(doc.language(), Some("rust"));
        assert_eq!(doc.encoding(), Encoding::Utf8);
        assert_eq!(doc.text(), "fn main() {}\n");
    }

    #[test]
    fn crlf_files_round_trip_byte_for_byte() {
        let original = b"line one\r\nline two\r\n";
        let doc = Document::from_bytes("/tmp/win.txt", original).unwrap();
        assert_eq!(doc.line_ending(), LineEnding::Crlf);
        assert_eq!(doc.to_bytes(), original.to_vec());
    }

    #[test]
    fn language_detection_covers_names_and_extensions() {
        assert_eq!(detect_language(Path::new("src/lib.rs")).as_deref(), Some("rust"));
        assert_eq!(detect_language(Path::new("Cargo.toml")).as_deref(), Some("toml"));
        assert_eq!(detect_language(Path::new("Dockerfile")).as_deref(), Some("dockerfile"));
        assert_eq!(detect_language(Path::new("a/b/app.TSX")).as_deref(), Some("typescriptreact"));
        assert_eq!(detect_language(Path::new("mystery.xyzzy")), None);
        assert_eq!(detect_language(Path::new("no_extension")), None);
    }

    #[test]
    fn add_cursor_keeps_the_existing_ones() {
        let mut doc = Document::from_str("a\nb\nc");
        doc.set_caret(0);
        doc.add_cursor(2);
        doc.add_cursor(4);
        assert_eq!(doc.selections().len(), 3);
        doc.insert_at_cursors("-", false).unwrap();
        assert_eq!(doc.text(), "-a\n-b\n-c");
    }

    #[test]
    fn word_at_cursor_finds_the_identifier_being_typed() {
        let mut doc = Document::from_str("let value = other;");
        doc.set_caret(9);
        assert_eq!(doc.word_at_cursor(), Range::new(4, 9));
    }
}
