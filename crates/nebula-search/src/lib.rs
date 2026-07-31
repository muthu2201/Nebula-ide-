//! # nebula-search
//!
//! Two distinct kinds of search, which users conflate and implementations must
//! not:
//!
//! * **Content search** ([`content`]) — "find every occurrence of this regex in
//!   the project". Built on the same `grep-*` crates as ripgrep, run across a
//!   rayon pool, and bounded so a pathological pattern cannot hang the UI.
//! * **Fuzzy matching** ([`fuzzy`]) — "I typed `mnrs`, show me `src/main.rs`".
//!   Built on `nucleo-matcher`, the matcher behind Helix's picker.
//!
//! Both are cancellable, because the user is typing and every keystroke
//! invalidates the query that was in flight.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod content;
pub mod fuzzy;

pub use content::{ContentSearcher, Match, SearchQuery, SearchResults};
pub use fuzzy::{FuzzyMatcher, FuzzyResult};

/// Errors from the search layer.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The pattern was not a valid regular expression.
    #[error("invalid search pattern: {0}")]
    BadPattern(String),

    /// A filesystem operation failed.
    #[error(transparent)]
    Vfs(#[from] nebula_vfs::VfsError),

    /// Reading a file during the search failed.
    #[error("io error while searching {path}: {source}")]
    Io {
        /// The file being searched.
        path: std::path::PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// Convenience result alias.
pub type Result<T, E = SearchError> = std::result::Result<T, E>;
