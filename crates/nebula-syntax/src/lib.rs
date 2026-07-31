//! # nebula-syntax
//!
//! Incremental parsing and highlighting built on tree-sitter.
//!
//! The central claim of this layer is that **editing a large file does not
//! re-parse it**. A keystroke produces a [`nebula_core::Transaction`]; that
//! transaction is translated into a tree-sitter `InputEdit`, tree-sitter reuses
//! every unaffected subtree, and the resulting parse costs time proportional to
//! the size of the change rather than the size of the file. Everything else
//! here — highlighting, symbols, structural selection — reads that one tree.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod grammar;
pub mod highlight;
pub mod symbols;
pub mod tree;

pub use grammar::{Grammar, GrammarRegistry};
pub use highlight::{HighlightKind, HighlightSpan, Highlighter};
pub use symbols::{Symbol, SymbolKind};
pub use tree::SyntaxTree;

/// Errors from the syntax layer.
#[derive(Debug, thiserror::Error)]
pub enum SyntaxError {
    /// No grammar is registered for the requested language.
    #[error("no grammar registered for language `{0}`")]
    UnknownLanguage(String),

    /// tree-sitter rejected the grammar, almost always an ABI mismatch between
    /// the grammar crate and the tree-sitter runtime.
    #[error("grammar for `{language}` is incompatible with this tree-sitter runtime: {source}")]
    IncompatibleGrammar {
        /// The language whose grammar failed to load.
        language: String,
        /// The underlying tree-sitter error.
        #[source]
        source: tree_sitter::LanguageError,
    },

    /// A highlight or symbol query failed to compile.
    #[error("query for `{language}` failed to compile: {source}")]
    BadQuery {
        /// The language whose query failed.
        language: String,
        /// The underlying tree-sitter error.
        #[source]
        source: tree_sitter::QueryError,
    },

    /// The parser returned no tree. tree-sitter only does this when parsing is
    /// cancelled or the timeout expires.
    #[error("parsing `{0}` did not produce a tree (cancelled or timed out)")]
    ParseFailed(String),

    /// An offset conversion against the buffer failed.
    #[error(transparent)]
    Core(#[from] nebula_core::CoreError),
}

/// Convenience result alias.
pub type Result<T, E = SyntaxError> = std::result::Result<T, E>;
