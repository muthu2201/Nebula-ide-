//! # nebula-index
//!
//! The repo map: a ranked, token-budgeted summary of a codebase, built so that
//! a model asked about a 4 000-file project can be shown the twenty files that
//! actually matter.
//!
//! ## How the ranking works
//!
//! Every file is a node. Every reference from file A to a symbol defined in
//! file B is an edge A → B. That graph is exactly the shape PageRank was
//! designed for, and the intuition transfers cleanly: a file that many other
//! files depend on is structurally important, and importance flows transitively
//! — being depended on by an important file counts for more.
//!
//! The ranking is **personalised**. The files the user currently has open seed
//! the random-surfer distribution, so the map is not a static "most important
//! files in the repo" list but "most important files *relative to what you are
//! working on right now*".
//!
//! ## Why not just embed everything
//!
//! Semantic search ([`nebula_vector`](https://docs.rs/nebula-vector)) answers
//! "what code is about this topic". The repo map answers a different question —
//! "what is the shape of this codebase" — and the two are complementary. The AI
//! context builder uses both.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod graph;
pub mod map;
pub mod pagerank;
pub mod tokens;

pub use graph::{FileNode, SymbolGraph};
pub use map::{RepoMap, RepoMapOptions};
pub use pagerank::{PageRank, PageRankConfig};

/// Errors from the index layer.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Reading the project failed.
    #[error(transparent)]
    Vfs(#[from] nebula_vfs::VfsError),

    /// Parsing a file failed in a way that was not recoverable.
    #[error(transparent)]
    Syntax(#[from] nebula_syntax::SyntaxError),

    /// Serialising a cached index failed.
    #[error("index serialisation failed: {0}")]
    Serialization(String),
}

/// Convenience result alias.
pub type Result<T, E = IndexError> = std::result::Result<T, E>;
