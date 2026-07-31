//! # nebula-license
//!
//! Offline licence validation for the $10/month subscription.
//!
//! ## What this is honestly for
//!
//! Client-side licensing deters casual sharing. It does not stop a determined
//! person: the verifying key is in the binary, the check is in the binary, and
//! anyone willing to patch a binary can remove it. That is true of every DRM
//! scheme and pretending otherwise would lead to bad decisions — like making the
//! honest-user experience worse in pursuit of a guarantee that is not available.
//!
//! So the design optimises for the honest user:
//!
//! * **Offline first.** A signed machine file is verified locally against a
//!   hardcoded public key. No network call at launch, ever. A developer on a
//!   plane keeps working.
//! * **Generous grace.** An expired licence enters a grace period rather than
//!   locking the editor, because the common cause is a failed card renewal, not
//!   fraud.
//! * **Fingerprints that tolerate reality.** Hardware changes. A fingerprint
//!   built from several signals with a similarity threshold survives a RAM
//!   upgrade, where an exact match would lock the user out of software they paid
//!   for.
//!
//! The fingerprint is an HMAC of stable hardware identifiers, never the
//! identifiers themselves, so a licence file does not disclose anything about
//! the machine it was issued for.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod fingerprint;
pub mod license;

pub use fingerprint::{Fingerprint, HardwareId};
pub use license::{License, LicenseStatus, Plan, Validator};

/// Errors from the licence layer.
#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    /// The licence file is malformed.
    #[error("licence file is malformed: {0}")]
    Malformed(String),

    /// The signature does not verify against the issuer key.
    #[error("licence signature is not valid")]
    BadSignature,

    /// The licence was issued for a different machine.
    #[error("this licence was issued for a different machine")]
    WrongMachine,

    /// The licence has expired and the grace period is over.
    #[error("licence expired on {expired_on} and the {grace_days}-day grace period has passed")]
    Expired {
        /// When it expired.
        expired_on: String,
        /// How long the grace period was.
        grace_days: u32,
    },

    /// No licence is installed.
    #[error("no licence is installed")]
    Missing,

    /// An I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T, E = LicenseError> = std::result::Result<T, E>;
