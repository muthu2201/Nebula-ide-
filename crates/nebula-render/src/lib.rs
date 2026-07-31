//! # nebula-render
//!
//! Drawing, behind a backend-agnostic API.
//!
//! ## Why there are two backends
//!
//! The GPU path is the product: it is what makes a keystroke reach the screen in
//! single-digit milliseconds on a 200 000-line file. But a renderer that only
//! works on a GPU does not work on a VM without one, on a machine whose driver
//! has crashed, or over a remote session with no acceleration — and an editor
//! that will not start is worse than one that scrolls a little less smoothly.
//!
//! So [`Renderer`] is a trait with two implementations, and the fallback is not
//! a degraded feature set — it is the same [`scene::Scene`], rasterised on the
//! CPU with `tiny-skia`. Everything renders; only the frame path differs.
//!
//! ## The shape of a frame
//!
//! The editor builds a [`scene::Scene`]: a flat, ordered list of rectangles,
//! glyph runs and clip regions in logical pixels. Nothing in the scene knows
//! about GPUs, buffers or draw calls, which is what lets the same frame be
//! asserted pixel-for-pixel in a test and submitted to a swapchain in
//! production.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod backend;
pub mod cpu;
pub mod gpu;
pub mod scene;
pub mod text;

pub use backend::{Backend, Renderer, RendererKind, Surface};
pub use cpu::CpuRenderer;
pub use gpu::GpuRenderer;
pub use scene::{Color, Point, Quad, Rect, Scene, TextRun};
pub use text::{FontSystem, GlyphAtlas, ShapedLine};

/// Errors from the renderer.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No usable graphics adapter was found.
    #[error("no usable GPU adapter: {0}")]
    NoAdapter(String),

    /// The device could not be created.
    #[error("could not create a graphics device: {0}")]
    DeviceCreation(String),

    /// The surface could not be configured.
    #[error("could not configure the surface: {0}")]
    Surface(String),

    /// The frame could not be drawn.
    #[error("frame failed: {0}")]
    Frame(String),

    /// The requested size is not usable.
    #[error("invalid surface size {width}x{height}")]
    InvalidSize {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },

    /// Font loading or shaping failed.
    #[error("text error: {0}")]
    Text(String),
}

/// Convenience result alias.
pub type Result<T, E = RenderError> = std::result::Result<T, E>;

/// The performance budget the renderer is held to.
///
/// These are the numbers from the blueprint. They are stated here as constants
/// rather than as prose so the stress harness can assert against them rather
/// than against a number someone remembered.
pub mod budget {
    use std::time::Duration;

    /// Keystroke to a submitted frame, on a warm frame with a GPU.
    pub const KEYSTROKE_TO_PHOTON_GPU: Duration = Duration::from_millis(8);

    /// The same, on the CPU fallback. Looser, and deliberately so: the fallback
    /// exists to keep the editor usable, not to match the GPU.
    pub const KEYSTROKE_TO_PHOTON_CPU: Duration = Duration::from_millis(16);

    /// Cold start to an interactive window.
    pub const COLD_START: Duration = Duration::from_millis(500);

    /// Resident memory with a medium project open.
    pub const MEMORY_BYTES: u64 = 300 * 1024 * 1024;
}
