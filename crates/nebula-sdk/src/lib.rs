//! # nebula-sdk
//!
//! The extension developer's toolchain.
//!
//! A proprietary SDK only works if it is genuinely pleasant to use — that is the
//! whole bet of choosing a curated ecosystem over VS Code compatibility. So the
//! commands cover the entire lifecycle without the developer having to learn the
//! package format, the signing scheme or the WIT world by hand:
//!
//! * `new` scaffolds a project that compiles and passes notarisation as it
//!   stands;
//! * `build` compiles to `wasm32-wasip2` and packages;
//! * `test` runs the extension in a host identical to production, so "it worked
//!   in the harness" means something;
//! * `check` runs the same notarisation the registry will, locally, before an
//!   upload can fail;
//! * `publish` signs and uploads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod project;
pub mod scaffold;

pub use project::{BuildOutput, Project};

/// Errors from the SDK.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// The current directory is not an extension project.
    #[error("no `nebula.toml` found in {0} or any parent directory")]
    NotAProject(std::path::PathBuf),

    /// The project directory already exists.
    #[error("{0} already exists")]
    AlreadyExists(std::path::PathBuf),

    /// The Rust toolchain is missing the WebAssembly target.
    #[error(
        "the `{0}` target is not installed; run `rustup target add {0}`"
    )]
    MissingTarget(String),

    /// `cargo` could not be found.
    #[error("cargo was not found on PATH; install Rust from https://rustup.rs")]
    NoCargo,

    /// The build failed.
    #[error("the extension failed to build:\n{0}")]
    BuildFailed(String),

    /// The build produced no component.
    #[error("the build produced no `.wasm` file at {0}")]
    NoArtifact(std::path::PathBuf),

    /// The package is invalid.
    #[error(transparent)]
    Package(#[from] nebula_pkg::PkgError),

    /// The extension failed its checks.
    #[error("notarisation would reject this extension:\n{0}")]
    WouldBeRejected(String),

    /// Running a command failed.
    #[error(transparent)]
    Exec(#[from] nebula_exec::ExecError),

    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The upload failed.
    #[error("publishing failed: {0}")]
    Publish(String),
}

/// Convenience result alias.
pub type Result<T, E = SdkError> = std::result::Result<T, E>;

/// The target extensions compile to.
///
/// `wasip2` rather than `wasip1`: the Component Model is what the host loads,
/// and `wasip1` produces a core module the host will refuse.
pub const WASM_TARGET: &str = "wasm32-wasip2";
