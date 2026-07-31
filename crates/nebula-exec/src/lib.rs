//! # nebula-exec
//!
//! Running programs on the user's behalf — build commands, test suites, agent
//! tool calls, MCP servers.
//!
//! Every launch goes through [`Command`], which guarantees four things that a
//! bare `std::process::Command` does not:
//!
//! 1. **Confinement.** On Unix the sandbox policy is applied between `fork` and
//!    `exec`, so the child is confined before it runs a single instruction of
//!    the target binary.
//! 2. **A timeout.** A hung process is killed, along with its whole process
//!    group — killing only the direct child leaves orphaned grandchildren
//!    holding the terminal and the build lock.
//! 3. **Bounded output.** A program that writes gigabytes to stdout has its
//!    output truncated rather than exhausting memory.
//! 4. **A clean environment.** The child gets an explicit environment rather
//!    than inheriting whatever the editor was launched with, so a tool cannot
//!    read secrets that happen to be in the editor's environment.

#![warn(missing_docs)]

pub mod command;
pub mod limits;

pub use command::{Command, Output, Status};
pub use limits::ResourceLimits;

/// Errors from the execution layer.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The program could not be found on `PATH`.
    #[error("program `{0}` was not found")]
    NotFound(String),

    /// Spawning failed.
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        /// The program that failed to start.
        program: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// An I/O error while running or collecting output.
    #[error("io error while running `{program}`: {source}")]
    Io {
        /// The program being run.
        program: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The sandbox policy could not be applied, and the launch was refused.
    #[error("refusing to run unconfined: {0}")]
    SandboxRefused(String),

    /// Applying the policy failed outright.
    #[error(transparent)]
    Sandbox(#[from] nebula_sandbox::SandboxError),

    /// The working directory does not exist or is not a directory.
    #[error("working directory {0} is not usable")]
    BadWorkingDirectory(std::path::PathBuf),
}

/// Convenience result alias.
pub type Result<T, E = ExecError> = std::result::Result<T, E>;
