//! # nebula-core
//!
//! The text model at the bottom of Nebula IDE. Everything above this crate — the
//! renderer, the language servers, the agent — manipulates documents through the
//! types defined here.
//!
//! The design constraints come straight from the performance budget: a keystroke
//! must reach the GPU in under 8 ms on a warm frame, so every operation on the
//! hot path is O(log n) over a rope rather than O(n) over a `String`.
//!
//! ## Layers
//!
//! * [`TextBuffer`] — a rope with line-ending and encoding awareness.
//! * [`Selection`] / [`SelectionSet`] — multi-cursor state with automatic merging.
//! * [`Transaction`] — a set of edits applied atomically and losslessly invertible.
//! * [`History`] — undo/redo built on inverted transactions with time-based grouping.
//! * [`Document`] — the composition of all of the above plus version tracking.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod document;
pub mod edit;
pub mod encoding;
pub mod history;
pub mod position;
pub mod rope_ext;
pub mod selection;
pub mod text;
pub mod word;

pub use document::{Document, DocumentId, DocumentMeta};
pub use edit::{Edit, Transaction, TransactionResult};
pub use encoding::{Encoding, LineEnding};
pub use history::{History, HistoryEntry};
pub use position::{Position, Range};
pub use selection::{Selection, SelectionSet};
pub use text::TextBuffer;

/// Errors produced by the core text model.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// A byte or char offset pointed outside the buffer.
    #[error("offset {offset} is out of bounds (buffer length {len})")]
    OffsetOutOfBounds {
        /// The offending offset.
        offset: usize,
        /// The length of the buffer at the time of the call.
        len: usize,
    },

    /// A line index pointed outside the buffer.
    #[error("line {line} is out of bounds (buffer has {lines} lines)")]
    LineOutOfBounds {
        /// The offending line index.
        line: usize,
        /// The number of lines in the buffer.
        lines: usize,
    },

    /// A range had `start > end`.
    #[error("inverted range: start {start} > end {end}")]
    InvertedRange {
        /// Range start.
        start: usize,
        /// Range end.
        end: usize,
    },

    /// A transaction was applied to a document whose version had moved on.
    #[error("stale transaction: built against version {expected}, document is at {actual}")]
    StaleTransaction {
        /// Version the transaction was built against.
        expected: u64,
        /// The document's current version.
        actual: u64,
    },

    /// The bytes on disk were not valid text in any supported encoding.
    #[error("could not decode file as text: {0}")]
    Decode(String),
}

/// Convenience result alias for this crate.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
