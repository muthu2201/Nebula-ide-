//! Execution and resource limits for a guest.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use wasmtime::ResourceLimiter;

/// Bounds on how long and how hard a guest may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionLimits {
    /// Wall-clock deadline, enforced by epoch interruption.
    pub epoch_deadline: Duration,
    /// Deterministic instruction budget, or `None` to rely on epochs alone.
    ///
    /// Wasmtime measures epoch interruption at 2–3× faster than fuel, so fuel is
    /// off by default and turned on where determinism matters more than speed —
    /// notarisation, and reproducing a report about a misbehaving extension.
    pub fuel: Option<u64>,
    /// Maximum time a host call made on the guest's behalf may take.
    ///
    /// Separate because neither fuel nor epochs interrupt a host call: a guest
    /// blocked in `wasi:io/poll` is not executing Wasm at all.
    pub host_call_timeout: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::interactive()
    }
}

impl ExecutionLimits {
    /// Limits for work on the interactive path.
    ///
    /// The deadline is well inside the frame budget: an extension that
    /// contributes to a keystroke has to finish in single-digit milliseconds or
    /// the user feels it.
    pub fn interactive() -> Self {
        Self {
            epoch_deadline: Duration::from_millis(50),
            fuel: None,
            host_call_timeout: Duration::from_millis(200),
        }
    }

    /// Limits for background work: formatting, linting, indexing.
    pub fn background() -> Self {
        Self {
            epoch_deadline: Duration::from_secs(30),
            fuel: None,
            host_call_timeout: Duration::from_secs(10),
        }
    }

    /// Deterministic limits, for notarisation and reproducible reports.
    pub fn deterministic(fuel: u64) -> Self {
        Self {
            epoch_deadline: Duration::from_secs(60),
            fuel: Some(fuel),
            host_call_timeout: Duration::from_secs(5),
        }
    }

    /// Set the wall-clock deadline.
    pub fn deadline(mut self, deadline: Duration) -> Self {
        self.epoch_deadline = deadline;
        self
    }

    /// Set the instruction budget.
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = Some(fuel);
        self
    }

    /// How many epoch ticks the deadline corresponds to.
    ///
    /// The host's timer thread ticks once per [`EPOCH_TICK`]; the deadline is
    /// expressed in ticks, rounded up so a sub-tick deadline still gets one.
    pub fn epoch_ticks(&self) -> u64 {
        let ticks = self.epoch_deadline.as_millis() / EPOCH_TICK.as_millis().max(1);
        (ticks as u64).max(1)
    }
}

/// How often the host's epoch timer ticks.
///
/// Finer granularity means tighter deadlines and more timer wakeups. One
/// millisecond is fine enough for a 50 ms interactive budget while costing a
/// thousand wakeups a second on one thread.
pub const EPOCH_TICK: Duration = Duration::from_millis(1);

/// Caps on what a guest may allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCaps {
    /// Maximum linear memory, in bytes.
    pub max_memory_bytes: usize,
    /// Maximum table entries.
    pub max_table_elements: usize,
    /// Maximum concurrent instances.
    pub max_instances: usize,
    /// Maximum tables.
    pub max_tables: usize,
    /// Maximum memories.
    pub max_memories: usize,
}

impl Default for ResourceCaps {
    fn default() -> Self {
        Self {
            // Generous enough for a syntax-aware formatter holding a whole file,
            // small enough that a runaway extension cannot swap the machine.
            max_memory_bytes: 64 * 1024 * 1024,
            max_table_elements: 100_000,
            // Wasmtime's own defaults are 10 000 for each of these, which is far
            // more than any legitimate extension needs.
            max_instances: 64,
            max_tables: 16,
            max_memories: 4,
        }
    }
}

impl ResourceCaps {
    /// Tight caps, for an untrusted or newly-installed extension.
    pub fn strict() -> Self {
        Self {
            max_memory_bytes: 16 * 1024 * 1024,
            max_table_elements: 10_000,
            max_instances: 8,
            max_tables: 4,
            max_memories: 1,
        }
    }

    /// Set the memory cap.
    pub fn memory(mut self, bytes: usize) -> Self {
        self.max_memory_bytes = bytes;
        self
    }
}

/// A Wasmtime `ResourceLimiter` enforcing [`ResourceCaps`].
#[derive(Debug)]
pub struct StoreLimits {
    caps: ResourceCaps,
    /// Whether a request was refused, so the host can report *why* a guest
    /// trapped rather than just that it did.
    memory_refused: bool,
    table_refused: bool,
}

impl StoreLimits {
    /// A limiter enforcing `caps`.
    pub fn new(caps: ResourceCaps) -> Self {
        Self { caps, memory_refused: false, table_refused: false }
    }

    /// Whether a memory growth request was refused.
    pub fn hit_memory_limit(&self) -> bool {
        self.memory_refused
    }

    /// Whether a table growth request was refused.
    pub fn hit_table_limit(&self) -> bool {
        self.table_refused
    }

    /// The caps in force.
    pub fn caps(&self) -> &ResourceCaps {
        &self.caps
    }
}

impl ResourceLimiter for StoreLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.caps.max_memory_bytes;
        if !allowed {
            self.memory_refused = true;
            tracing::debug!(
                desired,
                limit = self.caps.max_memory_bytes,
                "refusing a guest memory growth request"
            );
        }
        // Returning `Ok(false)` refuses the growth, which the guest observes as
        // a failed `memory.grow`. Returning an error would abort the whole
        // store, which is a harsher outcome than the guest asking for too much
        // warrants.
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.caps.max_table_elements;
        if !allowed {
            self.table_refused = true;
        }
        Ok(allowed)
    }

    fn instances(&self) -> usize {
        self.caps.max_instances
    }

    fn tables(&self) -> usize {
        self.caps.max_tables
    }

    fn memories(&self) -> usize {
        self.caps.max_memories
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interactive_deadline_fits_inside_a_frame_budget() {
        let limits = ExecutionLimits::interactive();
        assert!(
            limits.epoch_deadline <= Duration::from_millis(50),
            "an extension on the keystroke path must not cost a frame"
        );
        assert!(limits.fuel.is_none(), "epochs are 2-3x faster and are the default");
    }

    #[test]
    fn background_limits_are_more_generous_than_interactive_ones() {
        assert!(
            ExecutionLimits::background().epoch_deadline
                > ExecutionLimits::interactive().epoch_deadline
        );
    }

    #[test]
    fn deterministic_limits_carry_fuel() {
        let limits = ExecutionLimits::deterministic(1_000_000);
        assert_eq!(limits.fuel, Some(1_000_000));
    }

    #[test]
    fn epoch_ticks_round_up_so_a_short_deadline_still_gets_one() {
        let limits = ExecutionLimits::interactive().deadline(Duration::from_micros(500));
        assert_eq!(limits.epoch_ticks(), 1, "a sub-tick deadline must not become zero");

        let limits = ExecutionLimits::interactive().deadline(Duration::from_millis(50));
        assert_eq!(limits.epoch_ticks(), 50);
    }

    #[test]
    fn a_zero_deadline_still_yields_one_tick() {
        let limits = ExecutionLimits::interactive().deadline(Duration::ZERO);
        assert_eq!(limits.epoch_ticks(), 1, "zero would trap before the guest ran at all");
    }

    #[test]
    fn default_caps_are_far_below_wasmtimes_own() {
        let caps = ResourceCaps::default();
        // Wasmtime defaults to 10 000 instances, tables and memories.
        assert!(caps.max_instances < 10_000);
        assert!(caps.max_tables < 10_000);
        assert!(caps.max_memories < 10_000);
    }

    #[test]
    fn strict_caps_are_tighter_than_default_ones() {
        let strict = ResourceCaps::strict();
        let default = ResourceCaps::default();
        assert!(strict.max_memory_bytes < default.max_memory_bytes);
        assert!(strict.max_instances < default.max_instances);
    }

    #[test]
    fn the_limiter_permits_growth_within_the_cap() {
        let mut limits = StoreLimits::new(ResourceCaps::default().memory(1024 * 1024));
        assert!(limits.memory_growing(0, 512 * 1024, None).unwrap());
        assert!(limits.memory_growing(512 * 1024, 1024 * 1024, None).unwrap());
        assert!(!limits.hit_memory_limit());
    }

    #[test]
    fn the_limiter_refuses_growth_beyond_the_cap_and_records_it() {
        let mut limits = StoreLimits::new(ResourceCaps::default().memory(1024 * 1024));

        let allowed = limits.memory_growing(0, 2 * 1024 * 1024, None).unwrap();
        assert!(!allowed, "growth past the cap must be refused");
        assert!(
            limits.hit_memory_limit(),
            "the refusal must be recorded so the host can explain the trap"
        );
    }

    #[test]
    fn refusing_growth_does_not_abort_the_store() {
        // `Ok(false)` refuses the request; `Err` would kill the whole store,
        // which is harsher than a guest asking for too much deserves.
        let mut limits = StoreLimits::new(ResourceCaps::strict());
        assert!(limits.memory_growing(0, usize::MAX, None).is_ok());
    }

    #[test]
    fn table_growth_is_capped_too() {
        let mut caps = ResourceCaps::default();
        caps.max_table_elements = 100;
        let mut limits = StoreLimits::new(caps);

        assert!(limits.memory_growing(0, 1024, None).unwrap());
        assert!(limits.table_growing(0, 50, None).unwrap());
        assert!(!limits.table_growing(50, 500, None).unwrap());
        assert!(limits.hit_table_limit());
    }

    #[test]
    fn limits_round_trip_through_serde() {
        let limits = ExecutionLimits::deterministic(5_000);
        let json = serde_json::to_string(&limits).unwrap();
        assert_eq!(serde_json::from_str::<ExecutionLimits>(&json).unwrap(), limits);

        let caps = ResourceCaps::strict();
        let json = serde_json::to_string(&caps).unwrap();
        assert_eq!(serde_json::from_str::<ResourceCaps>(&json).unwrap(), caps);
    }
}
