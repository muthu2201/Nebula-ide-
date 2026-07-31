//! # nebula-vector
//!
//! Approximate nearest-neighbour search over embeddings, used by the AI context
//! builder to find code that is *semantically* related to what the user is
//! working on rather than merely lexically similar.
//!
//! Two pieces:
//!
//! * [`hnsw::Hnsw`] — a Hierarchical Navigable Small World index. Exact search
//!   over 100 000 vectors costs a full linear scan on every keystroke; HNSW
//!   turns that into a logarithmic graph walk with recall above 95% at default
//!   parameters.
//! * [`embed::Embedder`] — the port that turns text into vectors. The
//!   implementation that ships in-process is [`embed::HashingEmbedder`]; a
//!   neural embedder is loaded from a downloaded model through the same trait.
//!
//! The index is deterministic: level assignment uses a seeded PRNG, so building
//! the same corpus twice produces the same graph. That is what makes the
//! recall tests below meaningful rather than flaky.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod embed;
pub mod hnsw;

pub use embed::{Embedder, HashingEmbedder};
pub use hnsw::{Hnsw, HnswConfig, Metric, Neighbor};

/// Errors from the vector layer.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    /// A vector's length did not match the index dimensionality.
    #[error("expected a {expected}-dimensional vector, got {actual}")]
    DimensionMismatch {
        /// The index's dimensionality.
        expected: usize,
        /// What was supplied.
        actual: usize,
    },

    /// A vector contained NaN or infinity.
    ///
    /// Rejected at insert time: a single NaN poisons every distance comparison
    /// it takes part in and silently corrupts the graph.
    #[error("vector contains a non-finite value at index {index}")]
    NonFinite {
        /// Position of the offending component.
        index: usize,
    },

    /// Serialising or deserialising the index failed.
    #[error("index serialisation failed: {0}")]
    Serialization(String),

    /// Reading or writing the index file failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T, E = VectorError> = std::result::Result<T, E>;
