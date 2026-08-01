//! # nebula-agent
//!
//! The agent loop, and the safety machinery around it.
//!
//! ## The threat this crate is built against
//!
//! An agent reads untrusted text — a file in the repository, a web page, an MCP
//! tool result, a dependency's README — and then takes actions with the user's
//! authority. Prompt injection is OWASP's number one risk for LLM applications
//! for the second consecutive edition, and there is no single fix. What works is
//! defence in depth, and specifically one principle that shapes this whole
//! crate:
//!
//! > **Authorisation is enforced at a boundary the model cannot reach.**
//!
//! The model can be talked into asking for anything. It cannot be talked into
//! having the capability, because the capability check is not in the prompt, not
//! in the tool description, and not in any string the model influences — it is
//! in [`capability::GrantSet`], decided before the loop starts, and enforced in
//! Rust between the model's request and the tool's execution. Underneath that
//! sits the OS sandbox, which enforces the same boundary even if this layer has
//! a bug.
//!
//! The other layers are: input filtering of untrusted content
//! ([`injection`]), task-scoped grants rather than ambient authority, human
//! approval for anything destructive, and a tamper-evident [`audit`] log so that
//! whatever did happen can be reconstructed afterwards.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod capability;
pub mod injection;
pub mod loop_;
pub mod tools;

pub use audit::{AuditEntry, AuditLog, AuditOutcome};
pub use capability::{Capability, Grant, GrantSet};
pub use injection::{InjectionFinding, InjectionScanner, Severity};
pub use loop_::{Agent, AgentConfig, AgentEvent, TurnResult};
pub use tools::{Tool, ToolContext, ToolOutcome, ToolRegistry};

/// Errors from the agent layer.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The model asked for a tool that is not registered.
    #[error("no such tool: `{0}`")]
    UnknownTool(String),

    /// The model asked for something it was not granted.
    #[error("`{tool}` requires the {capability} capability, which was not granted for this task")]
    NotGranted {
        /// The tool that was refused.
        tool: String,
        /// The capability it needed.
        capability: String,
    },

    /// The user declined an approval prompt.
    #[error("`{0}` was declined")]
    Declined(String),

    /// A tool's arguments did not match its schema.
    #[error("invalid arguments for `{tool}`: {detail}")]
    InvalidArguments {
        /// The tool.
        tool: String,
        /// What was wrong.
        detail: String,
    },

    /// The loop hit its turn limit without finishing.
    #[error("the agent did not finish within {0} turns")]
    TurnLimit(usize),

    /// The model provider failed.
    #[error(transparent)]
    Model(#[from] nebula_ai::AiError),

    /// A tool failed to execute.
    #[error("tool `{tool}` failed: {message}")]
    ToolFailed {
        /// The tool.
        tool: String,
        /// Why.
        message: String,
    },

    /// A filesystem operation failed.
    #[error(transparent)]
    Vfs(#[from] nebula_vfs::VfsError),

    /// Writing the audit log failed.
    ///
    /// Fatal on purpose: an action that cannot be recorded must not be taken.
    #[error("audit log write failed: {0}")]
    Audit(String),

    /// The task was cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// Convenience result alias.
pub type Result<T, E = AgentError> = std::result::Result<T, E>;

/// Plan prompt-cache breakpoints for an agent turn.
///
/// An agent loop is the case prompt caching was designed for: the system prompt
/// and tool definitions are byte-identical on every turn, and the conversation
/// prefix only ever grows. Long-session TTLs pay for themselves many times over
/// here, which is why `long_session` is always true.
pub(crate) fn cache_plan(message_count: usize, has_tools: bool) -> nebula_ai::cache::CachePlan {
    nebula_ai::cache::CachePlan::for_conversation(message_count, true, has_tools, true)
}
