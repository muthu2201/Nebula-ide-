//! # nebula-mcp
//!
//! A Model Context Protocol client built around the **2026-07-28** revision,
//! with a compatibility layer for the revisions still in their deprecation
//! window.
//!
//! ## Why the version adapter exists
//!
//! 2026-07-28 is a large breaking release. It retired the
//! `initialize`/`initialized` handshake and the `Mcp-Session-Id` header in
//! favour of a **stateless core**: every request carries its own protocol
//! version, client identity and capabilities in `_meta`, so any request can
//! land on any server instance behind a round-robin load balancer. It also
//! replaced held-open server→client streams with **Multi Round-Trip Requests**,
//! and deprecated Sampling, Roots, Logging and the HTTP+SSE transport.
//!
//! Servers will be spread across revisions for at least the twelve-month
//! offramp the feature-lifecycle policy mandates. So nothing here hard-codes a
//! revision: [`version::ProtocolVersion`] is negotiated per server, and
//! [`client::Client`] speaks whichever of the three supported revisions the
//! server does.
//!
//! ## Layout
//!
//! * [`protocol`] — wire types, shared across revisions.
//! * [`version`] — revision constants, capability gating and negotiation.
//! * [`transport`] — stdio and Streamable HTTP.
//! * [`client`] — the client itself, including MRTR handling.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod protocol;
pub mod transport;
pub mod version;

pub use client::{Client, ClientConfig, ServerHandle};
pub use protocol::{
    Content, JsonRpcError, JsonRpcRequest, JsonRpcResponse, Prompt, Resource, Tool, ToolResult,
};
pub use transport::{StdioTransport, StreamableHttpTransport, Transport};
pub use version::{Capabilities, ProtocolVersion};

/// Errors from the MCP layer.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The transport failed.
    #[error("transport error: {0}")]
    Transport(String),

    /// The server returned a JSON-RPC error.
    #[error("server error {code}: {message}")]
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable message.
        message: String,
        /// Any structured data the server attached.
        data: Option<serde_json::Value>,
    },

    /// A response could not be parsed.
    #[error("malformed response: {0}")]
    Protocol(String),

    /// The server does not support a feature the caller asked for.
    #[error("server does not support {feature} (protocol {version})")]
    Unsupported {
        /// The feature requested.
        feature: String,
        /// The negotiated protocol revision.
        version: String,
    },

    /// The server did not answer in time.
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// The elicitation/sampling round trip exceeded its retry budget.
    #[error("multi-round-trip request exceeded {0} rounds without resolving")]
    TooManyRounds(usize),

    /// Launching or talking to a stdio server failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON encoding or decoding error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Convenience result alias.
pub type Result<T, E = McpError> = std::result::Result<T, E>;
