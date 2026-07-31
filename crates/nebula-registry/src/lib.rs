//! # nebula-registry
//!
//! The curated extension marketplace.
//!
//! ## Why curated
//!
//! An open registry that serves whatever is uploaded is a supply-chain
//! liability: extensions run on developers' machines, next to their source code
//! and their credentials. The trade the blueprint makes deliberately — following
//! Zed and Figma rather than the VS Code marketplace — is slower third-party
//! adoption in exchange for being able to say something true about what is
//! served.
//!
//! So publishing is a gate, not an upload:
//!
//! 1. the package's signature must verify against a **registered publisher key**
//!    — not merely against the key inside the package, which anyone can mint;
//! 2. [`nebula_pkg::notarize`] must not reject it, which among other things
//!    means the binary cannot import capabilities its manifest hides;
//! 3. anything requesting a capability that reaches outside the editor is held
//!    for human review rather than published automatically.
//!
//! Only after all three does a version become downloadable.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod store;

pub use api::router;
pub use store::{PublishOutcome, Publisher, Registry, RegistryEntry};

/// Errors from the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The package could not be read.
    #[error("invalid package: {0}")]
    InvalidPackage(String),

    /// The signature is not from a registered publisher.
    #[error("the package is not signed by a registered publisher key")]
    UnknownPublisher,

    /// The publisher is not allowed to publish under this identifier.
    ///
    /// Namespace ownership is what stops one publisher shipping an update to
    /// someone else's extension.
    #[error("publisher `{publisher}` does not own the namespace `{namespace}`")]
    NamespaceNotOwned {
        /// Who tried.
        publisher: String,
        /// What they tried to publish under.
        namespace: String,
    },

    /// The version already exists.
    ///
    /// Versions are immutable: an installed extension must not change under a
    /// user who pinned it.
    #[error("{id} version {version} is already published and versions are immutable")]
    VersionExists {
        /// The extension.
        id: String,
        /// The version.
        version: String,
    },

    /// The version is not newer than what is already published.
    #[error("{id} version {version} is not newer than the published {latest}")]
    VersionNotNewer {
        /// The extension.
        id: String,
        /// What was offered.
        version: String,
        /// What is already there.
        latest: String,
    },

    /// Notarisation rejected the package.
    #[error("notarisation rejected the package: {0}")]
    Rejected(String),

    /// Nothing is published under that identifier.
    #[error("no extension `{0}` is published")]
    NotFound(String),
}

/// Convenience result alias.
pub type Result<T, E = RegistryError> = std::result::Result<T, E>;
