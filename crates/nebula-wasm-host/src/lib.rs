//! # nebula-wasm-host
//!
//! Ring 1 of the isolation model: extensions run as WebAssembly components in
//! Wasmtime, and can reach exactly what the WIT world they were compiled
//! against grants them.
//!
//! ## Why the Component Model rather than core modules
//!
//! A core WebAssembly module exchanges raw memory offsets with its host, which
//! means the host must trust the guest's arithmetic and the guest can hand back
//! a pointer to anything in its linear memory. A **component** exchanges typed
//! values across an interface described in WIT: it imports and exports
//! functions, never memory regions. That is a materially stronger boundary, and
//! it is why the extension API is defined as a WIT world.
//!
//! ## Bounding execution
//!
//! A sandbox that cannot be interrupted is not a sandbox — an extension with an
//! infinite loop freezes the editor. Wasmtime offers two mechanisms and Nebula
//! uses both, because they solve different problems:
//!
//! * **Epoch interruption** ([`limits::ExecutionLimits::epoch_deadline`]) is
//!   wall-clock based and driven by a background timer. Wasmtime measures it at
//!   2–3× faster than fuel, so it is the default for interactive work. It is
//!   non-deterministic, and `Store::set_epoch_deadline` must be called before
//!   running or the guest traps immediately.
//! * **Fuel** ([`limits::ExecutionLimits::fuel`]) is a deterministic instruction
//!   budget. Slower, but reproducible — which is what the notarisation pipeline
//!   needs when it has to make the same decision about the same component twice.
//!
//! Neither interrupts a *host* call. A guest blocked in `wasi:io/poll` is not
//! executing Wasm and no amount of fuel will stop it, so every host function
//! that can block is async and carries its own timeout.
//!
//! Memory is capped separately through a [`limits::StoreLimits`] resource
//! limiter, because an extension that allocates until the machine swaps has
//! denied service without ever exhausting its instruction budget.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod host;
pub mod limits;
pub mod world;

pub use host::{ExtensionHost, ExtensionInstance};
pub use limits::{ExecutionLimits, ResourceCaps};
pub use world::{Capability, WitWorld, WorldVersion};

/// Errors from the extension host.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// The component could not be compiled.
    #[error("extension `{name}` failed to compile: {source}")]
    Compile {
        /// Which extension.
        name: String,
        /// Why.
        #[source]
        source: anyhow_lite::Error,
    },

    /// The component is not a Component Model binary.
    #[error("`{0}` is a core WebAssembly module, not a component; rebuild it for wasm32-wasip2")]
    NotAComponent(String),

    /// Instantiating the component failed.
    #[error("extension `{name}` failed to start: {detail}")]
    Instantiate {
        /// Which extension.
        name: String,
        /// Why.
        detail: String,
    },

    /// The guest trapped.
    #[error("extension `{name}` trapped: {detail}")]
    Trap {
        /// Which extension.
        name: String,
        /// What the trap was.
        detail: String,
    },

    /// The guest exhausted its instruction budget.
    #[error("extension `{name}` exceeded its instruction budget of {fuel} units")]
    OutOfFuel {
        /// Which extension.
        name: String,
        /// The budget it was given.
        fuel: u64,
    },

    /// The guest exceeded its wall-clock deadline.
    #[error("extension `{name}` exceeded its {millis} ms deadline")]
    DeadlineExceeded {
        /// Which extension.
        name: String,
        /// The deadline it was given.
        millis: u64,
    },

    /// The guest tried to allocate beyond its cap.
    #[error("extension `{name}` exceeded its {limit} byte memory limit")]
    OutOfMemory {
        /// Which extension.
        name: String,
        /// The cap.
        limit: usize,
    },

    /// The component requests a capability it was not granted.
    #[error("extension `{name}` requests the `{capability}` capability, which is not granted")]
    CapabilityNotGranted {
        /// Which extension.
        name: String,
        /// What it wanted.
        capability: String,
    },

    /// The component targets a WIT world version this host does not implement.
    #[error("extension `{name}` targets world version {wanted}, but this host provides {available}")]
    IncompatibleWorld {
        /// Which extension.
        name: String,
        /// What it wants.
        wanted: String,
        /// What is available.
        available: String,
    },

    /// The component's exports do not match the world.
    #[error("extension `{name}` does not export `{export}`")]
    MissingExport {
        /// Which extension.
        name: String,
        /// The missing export.
        export: String,
    },

    /// Reading the component failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A minimal stand-in for `anyhow::Error` so this crate's public error type does
/// not force `anyhow` on its dependents.
pub mod anyhow_lite {
    /// An opaque error carrying a message and its source chain, flattened.
    #[derive(Debug)]
    pub struct Error(String);

    impl Error {
        /// Wrap any error, flattening its source chain into the message.
        pub fn new(error: impl std::fmt::Display) -> Self {
            Self(error.to_string())
        }
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for Error {}
}

/// Convenience result alias.
pub type Result<T, E = WasmError> = std::result::Result<T, E>;
