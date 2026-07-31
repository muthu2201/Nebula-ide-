//! # nebula-pkg
//!
//! The extension package format, its signatures, and the notarisation checks a
//! package must pass before the marketplace will serve it.
//!
//! ## The shape of a package
//!
//! A `.nbx` is a gzipped tar containing:
//!
//! ```text
//! manifest.toml      the declared identity, version and capabilities
//! extension.wasm     the Component Model binary
//! signature.json     an Ed25519 signature over a digest of the above
//! assets/            themes, grammars, icons
//! ```
//!
//! ## Why the manifest is not trusted
//!
//! The manifest *declares* capabilities, and the notarisation scanner
//! ([`notarize`]) reads the component's **actual imports** and compares. A
//! package whose binary imports more than its manifest admits is rejected, so
//! the manifest a user is shown at install time is the truth rather than a
//! claim.
//!
//! ## What signing does and does not prove
//!
//! A signature proves the package came from the holder of a key and has not
//! been altered since. It says nothing about whether the code is safe — that is
//! what notarisation and human review are for. Both are required.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod manifest;
pub mod notarize;
pub mod package;
pub mod signature;

pub use manifest::{Author, Manifest};
pub use notarize::{NotarizationReport, NotarizationVerdict, notarize};
pub use package::{Package, PackageContents};
pub use signature::{KeyPair, PublicKey, Signature, SigningError};

/// Errors from the package layer.
#[derive(Debug, thiserror::Error)]
pub enum PkgError {
    /// The manifest is missing or malformed.
    #[error("invalid manifest: {0}")]
    Manifest(String),

    /// The archive is malformed.
    #[error("malformed package: {0}")]
    Archive(String),

    /// A required file is missing from the archive.
    #[error("package is missing `{0}`")]
    MissingFile(String),

    /// The signature does not verify.
    #[error("signature verification failed: {0}")]
    Signature(#[from] SigningError),

    /// The package failed notarisation.
    #[error("notarisation failed: {0}")]
    NotNotarized(String),

    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The component could not be inspected.
    #[error(transparent)]
    Wasm(#[from] nebula_wasm_host::WasmError),
}

/// Convenience result alias.
pub type Result<T, E = PkgError> = std::result::Result<T, E>;

/// The file extension for a Nebula extension package.
pub const PACKAGE_EXTENSION: &str = "nbx";

/// Names of the files a package must contain.
pub mod files {
    /// The manifest.
    pub const MANIFEST: &str = "manifest.toml";
    /// The component binary.
    pub const COMPONENT: &str = "extension.wasm";
    /// The detached signature.
    pub const SIGNATURE: &str = "signature.json";
    /// The assets directory prefix.
    pub const ASSETS: &str = "assets/";
}
