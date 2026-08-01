//! Resource limits applied to a child process.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Ceilings imposed on a child process.
///
/// These are defence in depth alongside the sandbox: a policy stops a tool
/// *reaching* things it should not, and limits stop it *consuming* more than it
/// should. A build that forks bombs is a denial of service on the user's
/// machine even though it never touched a file it was not allowed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Wall-clock limit. The process group is killed when it expires.
    pub timeout: Duration,
    /// Maximum captured bytes per output stream. Beyond this, output is
    /// truncated and flagged.
    pub max_output_bytes: usize,
    /// Maximum address space in bytes, or `None` for no limit.
    ///
    /// Applied with `RLIMIT_AS` on Unix. Note that this counts *virtual*
    /// address space, and some runtimes (notably the JVM and some Go builds)
    /// reserve far more than they commit, so this is left generous by default.
    pub max_memory_bytes: Option<u64>,
    /// Maximum number of processes/threads the child's user may create, or
    /// `None` for no limit. This is what stops a fork bomb.
    pub max_processes: Option<u64>,
    /// Maximum CPU seconds, or `None` for no limit.
    ///
    /// Distinct from `timeout`: a process sleeping for an hour uses no CPU, and
    /// a process spinning on four cores burns CPU seconds four times faster
    /// than wall-clock.
    pub max_cpu_seconds: Option<u64>,
    /// Maximum size of any file the child creates, or `None` for no limit.
    pub max_file_size_bytes: Option<u64>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            max_output_bytes: 8 * 1024 * 1024,
            max_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            max_processes: Some(512),
            max_cpu_seconds: Some(300),
            max_file_size_bytes: Some(2 * 1024 * 1024 * 1024),
        }
    }
}

impl ResourceLimits {
    /// Limits suited to a quick tool call: short timeout, small output.
    pub fn quick() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_output_bytes: 256 * 1024,
            max_cpu_seconds: Some(15),
            ..Default::default()
        }
    }

    /// Limits suited to a full build or test run.
    pub fn build() -> Self {
        Self {
            timeout: Duration::from_secs(900),
            max_output_bytes: 32 * 1024 * 1024,
            max_cpu_seconds: Some(3600),
            ..Default::default()
        }
    }

    /// Limits with everything unbounded except a wall-clock timeout.
    ///
    /// For long-lived children the editor supervises itself, such as a language
    /// server: an LSP process legitimately runs for hours and allocates freely.
    pub fn long_running() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60 * 24),
            max_output_bytes: 64 * 1024 * 1024,
            max_memory_bytes: None,
            max_processes: Some(512),
            max_cpu_seconds: None,
            max_file_size_bytes: None,
        }
    }

    /// Set the wall-clock timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the captured-output ceiling.
    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Set the memory ceiling.
    pub fn max_memory(mut self, bytes: Option<u64>) -> Self {
        self.max_memory_bytes = bytes;
        self
    }

    /// Apply the limits to the calling process.
    ///
    /// Called between `fork` and `exec`. Each limit that cannot be set is
    /// skipped rather than aborting the launch: a container may already impose
    /// a lower `RLIMIT_NPROC` than we ask for, and `setrlimit` then fails with
    /// `EPERM` because raising a hard limit needs privilege. Failing the launch
    /// in that case would make Nebula unusable inside a container that is
    /// *already* more restrictive than the policy asks for.
    #[cfg(unix)]
    pub fn apply_to_current_process(&self) -> std::io::Result<()> {
        use nix::sys::resource::{Resource, setrlimit};

        let set = |resource: Resource, value: u64, name: &str| {
            if let Err(err) = setrlimit(resource, value, value) {
                tracing::debug!(%err, limit = name, value, "could not set resource limit");
            }
        };

        if let Some(bytes) = self.max_memory_bytes {
            set(Resource::RLIMIT_AS, bytes, "RLIMIT_AS");
        }
        if let Some(count) = self.max_processes {
            set(Resource::RLIMIT_NPROC, count, "RLIMIT_NPROC");
        }
        if let Some(seconds) = self.max_cpu_seconds {
            set(Resource::RLIMIT_CPU, seconds, "RLIMIT_CPU");
        }
        if let Some(bytes) = self.max_file_size_bytes {
            set(Resource::RLIMIT_FSIZE, bytes, "RLIMIT_FSIZE");
        }
        // Core dumps from a sandboxed tool are never wanted and can be huge.
        set(Resource::RLIMIT_CORE, 0, "RLIMIT_CORE");
        Ok(())
    }

    /// No-op on platforms without `setrlimit`.
    #[cfg(not(unix))]
    pub fn apply_to_current_process(&self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded_in_every_dimension() {
        let limits = ResourceLimits::default();
        assert!(limits.timeout > Duration::ZERO);
        assert!(limits.max_output_bytes > 0);
        assert!(limits.max_memory_bytes.is_some());
        assert!(limits.max_processes.is_some(), "a fork bomb must be bounded by default");
    }

    #[test]
    fn presets_are_ordered_by_how_much_they_allow() {
        let quick = ResourceLimits::quick();
        let build = ResourceLimits::build();
        assert!(quick.timeout < build.timeout);
        assert!(quick.max_output_bytes < build.max_output_bytes);
    }

    #[test]
    fn the_long_running_preset_still_caps_process_count() {
        let limits = ResourceLimits::long_running();
        assert!(limits.max_memory_bytes.is_none(), "a language server allocates freely");
        assert!(
            limits.max_processes.is_some(),
            "but it still must not be able to fork-bomb the machine"
        );
    }

    #[test]
    fn builders_override_individual_fields() {
        let limits = ResourceLimits::default()
            .timeout(Duration::from_secs(5))
            .max_output(1024)
            .max_memory(None);
        assert_eq!(limits.timeout, Duration::from_secs(5));
        assert_eq!(limits.max_output_bytes, 1024);
        assert!(limits.max_memory_bytes.is_none());
        // Untouched fields keep their defaults.
        assert_eq!(limits.max_processes, ResourceLimits::default().max_processes);
    }

    #[test]
    fn limits_round_trip_through_serde() {
        let limits = ResourceLimits::build();
        let json = serde_json::to_string(&limits).unwrap();
        assert_eq!(serde_json::from_str::<ResourceLimits>(&json).unwrap(), limits);
    }
}
