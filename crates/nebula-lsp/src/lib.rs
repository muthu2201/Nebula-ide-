//! # nebula-lsp
//!
//! A Language Server Protocol client.
//!
//! LSP is JSON-RPC with an HTTP-style framing layer — `Content-Length` headers
//! followed by a JSON body — and a strict lifecycle: `initialize`, then
//! `initialized`, then everything else, then `shutdown` and `exit`. Getting the
//! framing or the lifecycle wrong produces a server that hangs rather than one
//! that complains, so both are handled here and tested directly.
//!
//! ## The offset problem
//!
//! LSP positions are line plus **UTF-16 code unit** column. Nebula's buffers
//! index characters. A file containing an emoji desynchronises a client that
//! conflates the two, and the symptom — completions inserted at the wrong place,
//! but only in files with non-BMP characters — is famously hard to track down.
//! Every position crossing this boundary goes through
//! [`nebula_core::TextBuffer::offset_to_lsp_position`] and its inverse.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod framing;
pub mod types;

pub use client::{LanguageServer, ServerConfig};
pub use framing::{FrameDecoder, encode_message};
pub use types::{CompletionItem, Diagnostic, DiagnosticSeverity, Location, Position, Range};

/// Errors from the LSP layer.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The server process could not be started.
    #[error("could not start language server `{program}`: {source}")]
    Spawn {
        /// The program that failed to start.
        program: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The message framing was malformed.
    #[error("malformed message framing: {0}")]
    Framing(String),

    /// The server returned a JSON-RPC error.
    #[error("language server error {code}: {message}")]
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// Message.
        message: String,
    },

    /// A response did not have the expected shape.
    #[error("unexpected response to `{method}`: {detail}")]
    Protocol {
        /// The method whose response was wrong.
        method: String,
        /// What was wrong with it.
        detail: String,
    },

    /// The server did not answer in time.
    #[error("`{method}` timed out after {timeout:?}")]
    Timeout {
        /// The method that timed out.
        method: String,
        /// The timeout that expired.
        timeout: std::time::Duration,
    },

    /// The server exited.
    #[error("language server exited")]
    Exited,

    /// A method was called before `initialize` completed.
    #[error("`{0}` was called before the server was initialised")]
    NotInitialized(String),

    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A buffer offset conversion failed.
    #[error(transparent)]
    Core(#[from] nebula_core::CoreError),
}

/// Convenience result alias.
pub type Result<T, E = LspError> = std::result::Result<T, E>;
