//! # nebula-stress
//!
//! The end-to-end stress harness.
//!
//! It generates a real multi-language project, opens it in a real editor,
//! delivers thousands of real keystrokes through the real keymap, draws a real
//! frame after each one, builds every index the editor relies on, then compiles
//! and executes the fixture's programs as real child processes and checks what
//! they printed.
//!
//! Nothing in here stands in for anything. That is the point: a harness that
//! substitutes a fake for the expensive part measures the fake.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod fixture;
pub mod harness;
pub mod report;

pub use fixture::{Fixture, Program};
pub use harness::{Budgets, Options, run};
pub use report::{PhaseReport, ProgramRun, Report};
