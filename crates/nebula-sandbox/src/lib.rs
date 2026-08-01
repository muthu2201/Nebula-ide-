//! # nebula-sandbox
//!
//! Ring 2 of Nebula's three-ring isolation model: OS-level confinement for
//! agent tools and untrusted MCP servers. (Ring 1 is the WASM capability
//! sandbox in `nebula-wasm-host`; ring 3 is a microVM for fully untrusted
//! generated code.)
//!
//! ## What this crate promises, and what it does not
//!
//! A [`Policy`] describes what a child process may touch: which directories it
//! may read, which it may write, whether it may reach the network. Applying a
//! policy is **irreversible and inherited** — that is the security property the
//! whole design leans on. On Linux, Landlock's own documentation is explicit
//! that "once a thread is landlocked, there is no way to remove its security
//! policy; only adding more restrictions is allowed."
//!
//! What it does *not* promise: a policy applied to the current process cannot
//! be un-applied for tests, so the tests here exercise policy construction and
//! child-process enforcement rather than self-confinement.
//!
//! ## Platform support
//!
//! | Platform | Mechanism | Status |
//! |----------|-----------|--------|
//! | Linux ≥ 5.13 | Landlock LSM + seccomp-bpf | enforced |
//! | Linux < 5.13 | — | [`Enforcement::Unsupported`], caller decides |
//! | macOS | Seatbelt (`sandbox_init`) | enforced, deprecated upstream |
//! | Windows | Job Objects + restricted token | resource limits enforced |
//!
//! Every platform reports what it actually achieved through
//! [`Enforcement`], so a caller can refuse to run a tool rather than silently
//! running it unconfined. Failing open without telling anyone is the one
//! outcome this crate will not produce.

#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod policy;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
pub mod windows;

pub use policy::{NetworkAccess, Policy, PolicyBuilder};

/// What confinement was actually achieved.
///
/// Returned rather than swallowed so callers can make a policy decision about
/// running unconfined, instead of finding out afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Enforcement {
    /// The full policy is in force.
    Full {
        /// Human-readable description of the mechanism, for the audit log.
        mechanism: String,
    },
    /// Some of the policy is in force, but not all of it.
    Partial {
        /// The mechanism that applied.
        mechanism: String,
        /// What could not be enforced, for the audit log and the UI.
        missing: Vec<String>,
    },
    /// No confinement is available on this system.
    Unsupported {
        /// Why.
        reason: String,
    },
}

impl Enforcement {
    /// Whether the full policy is in force.
    pub fn is_full(&self) -> bool {
        matches!(self, Enforcement::Full { .. })
    }

    /// Whether any confinement at all is in force.
    pub fn is_confined(&self) -> bool {
        !matches!(self, Enforcement::Unsupported { .. })
    }

    /// A one-line summary for the audit log.
    pub fn summary(&self) -> String {
        match self {
            Enforcement::Full { mechanism } => format!("confined via {mechanism}"),
            Enforcement::Partial { mechanism, missing } => {
                format!("partially confined via {mechanism}; not enforced: {}", missing.join(", "))
            }
            Enforcement::Unsupported { reason } => format!("UNCONFINED: {reason}"),
        }
    }
}

/// Errors from the sandbox layer.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The policy itself is invalid.
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),

    /// Applying the policy failed.
    #[error("failed to apply sandbox policy: {0}")]
    ApplyFailed(String),

    /// A path in the policy could not be resolved.
    #[error("policy path {path} could not be resolved: {source}")]
    BadPath {
        /// The offending path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// Convenience result alias.
pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

/// Apply `policy` to the **current thread and its future children**.
///
/// This is irreversible on every platform that supports it. It is intended to
/// be called between `fork` and `exec` (see `nebula-exec`), never in the
/// editor's own process.
///
/// # Platform behaviour
///
/// * Linux: installs a Landlock ruleset and, if the policy denies network
///   access, a seccomp filter blocking socket syscalls.
/// * macOS: compiles and applies a Seatbelt profile.
/// * Windows: returns [`Enforcement::Unsupported`] for filesystem scoping;
///   resource limits are applied by the launcher through a Job Object instead.
pub fn apply(policy: &Policy) -> Result<Enforcement> {
    policy.validate()?;

    #[cfg(target_os = "linux")]
    {
        linux::apply(policy)
    }

    #[cfg(target_os = "macos")]
    {
        macos::apply(policy)
    }

    #[cfg(windows)]
    {
        windows::apply(policy)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = policy;
        Ok(Enforcement::Unsupported {
            reason: format!("no sandbox backend for {}", std::env::consts::OS),
        })
    }
}

/// Whether this build has a sandbox backend that can enforce filesystem scoping.
pub fn is_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::landlock_abi().is_some()
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// A human-readable description of the sandbox backend on this system.
pub fn backend_description() -> String {
    #[cfg(target_os = "linux")]
    {
        match linux::landlock_abi() {
            Some(abi) => format!("Linux Landlock ABI v{abi} + seccomp-bpf"),
            None => "Linux (Landlock unavailable; kernel older than 5.13 or disabled)".to_string(),
        }
    }
    #[cfg(target_os = "macos")]
    {
        "macOS Seatbelt (sandbox_init)".to_string()
    }
    #[cfg(windows)]
    {
        "Windows Job Objects".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        format!("none ({})", std::env::consts::OS)
    }
}

/// Canonicalise a policy path, tolerating paths that do not exist.
pub(crate) fn resolve(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        // A rule naming a directory that does not exist yet is not an error —
        // it simply grants nothing until something is created there.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(source) => Err(SandboxError::BadPath { path: path.to_path_buf(), source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_reports_its_own_strength_honestly() {
        let full = Enforcement::Full { mechanism: "Landlock".into() };
        assert!(full.is_full());
        assert!(full.is_confined());

        let partial =
            Enforcement::Partial { mechanism: "Landlock".into(), missing: vec!["network".into()] };
        assert!(!partial.is_full());
        assert!(partial.is_confined());

        let none = Enforcement::Unsupported { reason: "old kernel".into() };
        assert!(!none.is_full());
        assert!(!none.is_confined(), "unsupported must never read as confined");
    }

    #[test]
    fn summaries_make_the_unconfined_case_loud() {
        let none = Enforcement::Unsupported { reason: "old kernel".into() };
        assert!(
            none.summary().contains("UNCONFINED"),
            "an operator scanning logs must not miss this"
        );
    }

    #[test]
    fn the_backend_describes_itself() {
        let description = backend_description();
        assert!(!description.is_empty());
        // Whatever the platform, the description must name it.
        assert!(
            description.contains("Landlock")
                || description.contains("Seatbelt")
                || description.contains("Job Objects")
                || description.contains("none"),
            "unexpected backend description: {description}"
        );
    }

    #[test]
    fn resolving_a_nonexistent_path_is_not_an_error() {
        let resolved = resolve(Path::new("/definitely/does/not/exist/anywhere")).unwrap();
        assert_eq!(resolved, PathBuf::from("/definitely/does/not/exist/anywhere"));
    }
}
