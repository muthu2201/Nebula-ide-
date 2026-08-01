//! # nebula-update
//!
//! Signed delta updates.
//!
//! ## The rule this crate exists to enforce
//!
//! **Nothing is written to the installation directory until its signature has
//! been verified against a key compiled into the running binary.** An updater
//! that downloads and then verifies is one bug away from being a remote code
//! execution vector, so the API is shaped to make that order the only one
//! available: [`Update::verify`] consumes the downloaded bytes and returns a
//! [`VerifiedUpdate`], and only a `VerifiedUpdate` can be applied.
//!
//! Delta patches keep the download small — a typical release changes a few
//! hundred kilobytes of a fifty-megabyte binary — but the *result* of applying a
//! patch is verified against the new release's own hash, not just the patch's.
//! Verifying only the patch would let a malicious patch of a tampered base
//! produce anything at all.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod delta;
pub mod manifest;

pub use delta::{Patch, apply_patch, make_patch};
pub use manifest::{
    Channel, Platform, Release, ReleaseManifest, Update, UpdateSigner, VerifiedUpdate,
};

use base64::Engine as _;
use ed25519_dalek::{Verifier, VerifyingKey};

/// Errors from the update layer.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The manifest could not be parsed.
    #[error("update manifest is malformed: {0}")]
    Manifest(String),

    /// The manifest's signature does not verify.
    #[error("update manifest signature is not valid")]
    BadManifestSignature,

    /// The downloaded bytes do not match the hash in the manifest.
    #[error("downloaded update does not match its published hash")]
    HashMismatch {
        /// What the manifest promised.
        expected: String,
        /// What arrived.
        actual: String,
    },

    /// Applying a delta patch failed.
    #[error("could not apply the delta patch: {0}")]
    Patch(String),

    /// No update is available for this platform.
    #[error("no build is published for {0}")]
    NoBuildForPlatform(String),

    /// The download failed.
    #[error("download failed: {0}")]
    Download(String),

    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T, E = UpdateError> = std::result::Result<T, E>;

/// The public key release manifests are signed with.
///
/// Replaced at release time by the build pipeline. A manifest signed by anything
/// else is refused, which is what stops an attacker who controls the update
/// server from shipping arbitrary code.
pub const RELEASE_SIGNING_KEY: &str = "Qm5lYnVsYS1kZXYtcmVsZWFzZS1rZXktbm90LXByb2Q=";

/// Verify an Ed25519 signature over `content`.
pub fn verify_signature(content: &[u8], signature: &str, public_key: &str) -> Result<()> {
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(public_key.trim())
        .map_err(|e| UpdateError::Manifest(format!("signing key is not base64: {e}")))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| UpdateError::Manifest("signing key must be 32 bytes".to_string()))?;
    let key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|e| UpdateError::Manifest(e.to_string()))?;

    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature.trim())
        .map_err(|_| UpdateError::BadManifestSignature)?;
    let signature_bytes: [u8; 64] =
        signature_bytes.try_into().map_err(|_| UpdateError::BadManifestSignature)?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);

    key.verify(content, &signature).map_err(|_| UpdateError::BadManifestSignature)
}

/// The blake3 hash of `bytes`, hex-encoded.
pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
