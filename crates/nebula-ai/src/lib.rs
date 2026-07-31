//! # nebula-ai
//!
//! Cloud model access, built around one architectural commitment: **the client
//! talks to the provider directly, and nothing passes through a Nebula server.**
//!
//! That is not a preference, it is the whole basis of the product's privacy
//! claim. A vendor that routes requests through its own backend — even only to
//! build the final prompt — has the user's code on its servers, and no policy
//! statement changes that. So there is no proxy here, no telemetry on prompt
//! content, and no server-side prompt assembly: [`anthropic::AnthropicProvider`]
//! opens a TLS connection from the user's machine to `api.anthropic.com` using
//! the user's own key, retrieved from the OS keychain.
//!
//! It is also what keeps Nebula on the right side of provider terms. Anthropic's
//! 2026 terms restrict using one subscription to authenticate API access on
//! behalf of third-party end users; BYOK sidesteps that entirely, because every
//! user authenticates as themselves and Nebula never resells a token.
//!
//! ## Layout
//!
//! * [`provider`] — the [`provider::ModelProvider`] port every backend implements.
//! * [`models`] — the model catalogue: context windows, pricing, quirks.
//! * [`anthropic`] — the Messages API adapter, including SSE streaming.
//! * [`keys`] — BYOK storage in the OS keychain.
//! * [`cache`] — prompt cache breakpoint placement.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anthropic;
pub mod cache;
pub mod keys;
pub mod models;
pub mod provider;

pub use anthropic::AnthropicProvider;
pub use cache::{CacheBreakpoint, CacheTtl};
pub use keys::{KeyStore, Provider};
pub use models::{Model, ModelInfo};
pub use provider::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, ModelProvider, Role, StopReason,
    StreamEvent, Usage,
};

/// Errors from the model layer.
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// No API key is configured for the provider.
    #[error("no API key stored for {0}; add one in Settings → Models")]
    NoApiKey(String),

    /// The OS keychain could not be reached.
    #[error("keychain error: {0}")]
    Keychain(String),

    /// The provider rejected the request.
    ///
    /// Carries the HTTP status so the caller can distinguish a bad key (401)
    /// from a rate limit (429) from an overloaded server (529), which need very
    /// different responses.
    #[error("{provider} returned HTTP {status}: {message}")]
    Api {
        /// Which provider.
        provider: String,
        /// HTTP status code.
        status: u16,
        /// The error message the provider supplied.
        message: String,
        /// The provider's error type string, when it sent one.
        error_type: Option<String>,
    },

    /// The request could not be sent.
    #[error("network error talking to {provider}: {message}")]
    Network {
        /// Which provider.
        provider: String,
        /// What went wrong.
        message: String,
    },

    /// The response could not be parsed.
    #[error("malformed response from {provider}: {detail}")]
    Protocol {
        /// Which provider.
        provider: String,
        /// What was wrong.
        detail: String,
    },

    /// The request exceeds the model's context window.
    #[error("request needs about {needed} tokens but {model} accepts {limit}")]
    ContextTooLarge {
        /// The model.
        model: String,
        /// Estimated tokens required.
        needed: usize,
        /// The model's limit.
        limit: usize,
    },

    /// A model identifier was not recognised.
    #[error("unknown model `{0}`")]
    UnknownModel(String),

    /// The request was cancelled by the caller.
    #[error("request cancelled")]
    Cancelled,
}

/// Convenience result alias.
pub type Result<T, E = AiError> = std::result::Result<T, E>;

impl AiError {
    /// Whether retrying the same request could plausibly succeed.
    ///
    /// Rate limits and transient server errors are worth retrying; an invalid
    /// key or a malformed request will fail identically every time, and retrying
    /// only burns the user's quota.
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::Api { status, .. } => {
                matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
            }
            AiError::Network { .. } => true,
            _ => false,
        }
    }

    /// Whether this error means the user's credentials are wrong.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, AiError::Api { status: 401 | 403, .. } | AiError::NoApiKey(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_failures_are_retryable_and_permanent_ones_are_not() {
        let rate_limited = AiError::Api {
            provider: "anthropic".into(),
            status: 429,
            message: "rate limited".into(),
            error_type: Some("rate_limit_error".into()),
        };
        assert!(rate_limited.is_retryable());

        let overloaded = AiError::Api {
            provider: "anthropic".into(),
            status: 529,
            message: "overloaded".into(),
            error_type: None,
        };
        assert!(overloaded.is_retryable());

        let bad_request = AiError::Api {
            provider: "anthropic".into(),
            status: 400,
            message: "temperature is not supported".into(),
            error_type: Some("invalid_request_error".into()),
        };
        assert!(!bad_request.is_retryable(), "retrying a malformed request wastes quota");

        assert!(AiError::Network { provider: "anthropic".into(), message: "reset".into() }
            .is_retryable());
    }

    #[test]
    fn auth_failures_are_identified_so_the_ui_can_prompt_for_a_key() {
        let unauthorized = AiError::Api {
            provider: "anthropic".into(),
            status: 401,
            message: "invalid x-api-key".into(),
            error_type: Some("authentication_error".into()),
        };
        assert!(unauthorized.is_auth_failure());
        assert!(!unauthorized.is_retryable());

        assert!(AiError::NoApiKey("anthropic".into()).is_auth_failure());
    }
}
