//! The backend-agnostic renderer interface.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::scene::Scene;

/// Which implementation is drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RendererKind {
    /// The wgpu backend.
    Gpu,
    /// The `tiny-skia` software backend.
    Cpu,
}

impl RendererKind {
    /// A name for the status bar and for logs.
    pub const fn name(&self) -> &'static str {
        match self {
            RendererKind::Gpu => "GPU",
            RendererKind::Cpu => "CPU (software)",
        }
    }

    /// The frame budget this backend is held to.
    pub const fn frame_budget(&self) -> std::time::Duration {
        match self {
            RendererKind::Gpu => crate::budget::KEYSTROKE_TO_PHOTON_GPU,
            RendererKind::Cpu => crate::budget::KEYSTROKE_TO_PHOTON_CPU,
        }
    }
}

/// Where a frame is drawn to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surface {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
}

impl Surface {
    /// A surface of the given device-pixel size.
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Number of pixels.
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// A rendered frame as RGBA8 pixels.
///
/// Returned by the CPU backend and by the GPU backend's readback path, so a
/// frame can be asserted pixel-for-pixel regardless of which one drew it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, row-major, no padding.
    pub pixels: Vec<u8>,
}

impl Framebuffer {
    /// The pixel at `(x, y)` as `(r, g, b, a)`.
    pub fn pixel(&self, x: u32, y: u32) -> Option<(u8, u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = ((y * self.width + x) * 4) as usize;
        Some((
            self.pixels[index],
            self.pixels[index + 1],
            self.pixels[index + 2],
            self.pixels[index + 3],
        ))
    }

    /// How many pixels differ from `other` by more than `tolerance` per channel.
    ///
    /// Antialiasing differs slightly between backends, so an exact comparison
    /// would be useless; a tolerance makes the comparison meaningful.
    pub fn differing_pixels(&self, other: &Framebuffer, tolerance: u8) -> usize {
        if self.width != other.width || self.height != other.height {
            return self.pixel_count().max(other.pixel_count());
        }
        self.pixels
            .chunks_exact(4)
            .zip(other.pixels.chunks_exact(4))
            .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > tolerance))
            .count()
    }

    /// Number of pixels.
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Something that can draw a [`Scene`].
pub trait Renderer: Send {
    /// Which implementation this is.
    fn kind(&self) -> RendererKind;

    /// Resize the target.
    fn resize(&mut self, surface: Surface) -> Result<()>;

    /// The current target size.
    fn surface(&self) -> Surface;

    /// Draw a scene, returning the pixels.
    fn render(&mut self, scene: &Scene) -> Result<Framebuffer>;

    /// A description of the backend, for the status bar and bug reports.
    fn describe(&self) -> String;

    /// Whether frames are drawn by real graphics hardware.
    ///
    /// False for the CPU backend, and *also* false for a GPU adapter that is
    /// itself a software rasteriser — lavapipe, llvmpipe and SwiftShader all
    /// report themselves as CPU devices. Those are common on CI runners and in
    /// virtual machines, and holding one to the hardware frame budget measures
    /// the emulator rather than the editor.
    fn is_hardware_accelerated(&self) -> bool {
        false
    }
}

/// Chooses a backend.
///
/// The GPU is tried first and the CPU is used when it is unavailable. The
/// fallback is not silent — [`Backend::selection_reason`] records why, so a user
/// whose editor is unexpectedly slow can find out that their driver failed to
/// initialise rather than guessing.
pub struct Backend {
    renderer: Box<dyn Renderer>,
    reason: String,
}

impl Backend {
    /// Select the best available backend for `surface`.
    pub fn select(surface: Surface) -> Result<Self> {
        match crate::gpu::GpuRenderer::new(surface) {
            Ok(renderer) => {
                let reason = format!("using the GPU backend: {}", renderer.describe());
                Ok(Self { renderer: Box::new(renderer), reason })
            }
            Err(error) => {
                tracing::warn!(%error, "no GPU backend available; falling back to software rendering");
                let renderer = crate::cpu::CpuRenderer::new(surface)?;
                Ok(Self {
                    reason: format!(
                        "using the software backend because the GPU was unavailable: {error}"
                    ),
                    renderer: Box::new(renderer),
                })
            }
        }
    }

    /// Force the software backend.
    pub fn force_cpu(surface: Surface) -> Result<Self> {
        Ok(Self {
            renderer: Box::new(crate::cpu::CpuRenderer::new(surface)?),
            reason: "using the software backend because it was requested".to_string(),
        })
    }

    /// Why this backend was chosen.
    pub fn selection_reason(&self) -> &str {
        &self.reason
    }

    /// The renderer.
    pub fn renderer(&mut self) -> &mut dyn Renderer {
        self.renderer.as_mut()
    }

    /// Whether frames are drawn by real graphics hardware.
    ///
    /// False for the CPU backend, and also false for a GPU adapter that is
    /// itself a software rasteriser. See [`Renderer::is_hardware_accelerated`].
    pub fn is_hardware_accelerated(&self) -> bool {
        self.renderer.is_hardware_accelerated()
    }

    /// Which implementation is in use.
    pub fn kind(&self) -> RendererKind {
        self.renderer.kind()
    }

    /// Draw a scene.
    pub fn render(&mut self, scene: &Scene) -> Result<Framebuffer> {
        self.renderer.render(scene)
    }

    /// Resize the target.
    pub fn resize(&mut self, surface: Surface) -> Result<()> {
        self.renderer.resize(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framebuffer(width: u32, height: u32, fill: (u8, u8, u8, u8)) -> Framebuffer {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&[fill.0, fill.1, fill.2, fill.3]);
        }
        Framebuffer { width, height, pixels }
    }

    #[test]
    fn backends_name_themselves_and_carry_a_budget() {
        assert_eq!(RendererKind::Gpu.name(), "GPU");
        assert!(RendererKind::Cpu.name().contains("software"));
        assert!(
            RendererKind::Cpu.frame_budget() > RendererKind::Gpu.frame_budget(),
            "the fallback exists to stay usable, not to match the GPU"
        );
    }

    #[test]
    fn framebuffer_pixels_are_addressable() {
        let buffer = framebuffer(4, 3, (10, 20, 30, 255));
        assert_eq!(buffer.pixel(0, 0), Some((10, 20, 30, 255)));
        assert_eq!(buffer.pixel(3, 2), Some((10, 20, 30, 255)));
        assert_eq!(buffer.pixel(4, 0), None, "out of bounds must not panic");
        assert_eq!(buffer.pixel(0, 3), None);
        assert_eq!(buffer.pixel_count(), 12);
    }

    #[test]
    fn identical_framebuffers_do_not_differ() {
        let a = framebuffer(8, 8, (1, 2, 3, 255));
        assert_eq!(a.differing_pixels(&a, 0), 0);
    }

    #[test]
    fn the_comparison_tolerance_absorbs_antialiasing_differences() {
        // Backends antialias slightly differently; an exact comparison would be
        // useless for cross-backend assertions.
        let a = framebuffer(8, 8, (100, 100, 100, 255));
        let b = framebuffer(8, 8, (102, 100, 100, 255));

        assert_eq!(a.differing_pixels(&b, 0), 64, "with no tolerance every pixel differs");
        assert_eq!(a.differing_pixels(&b, 2), 0, "within tolerance they match");
    }

    #[test]
    fn differently_sized_framebuffers_differ_everywhere() {
        let a = framebuffer(8, 8, (0, 0, 0, 255));
        let b = framebuffer(4, 4, (0, 0, 0, 255));
        assert_eq!(a.differing_pixels(&b, 255), 64);
    }

    #[test]
    fn surfaces_report_their_pixel_count() {
        assert_eq!(Surface::new(1920, 1080).pixel_count(), 2_073_600);
    }

    #[test]
    fn forcing_the_cpu_backend_says_why() {
        let backend = Backend::force_cpu(Surface::new(64, 64)).unwrap();
        assert_eq!(backend.kind(), RendererKind::Cpu);
        assert!(backend.selection_reason().contains("requested"));
    }

    #[test]
    fn selection_records_its_reason_either_way() {
        // On a machine with no GPU this exercises the fallback; on one with a
        // GPU it exercises the happy path. Both must explain themselves, and
        // neither may fail.
        let backend = Backend::select(Surface::new(64, 64)).unwrap();
        let reason = backend.selection_reason();

        assert!(!reason.is_empty());
        match backend.kind() {
            RendererKind::Gpu => assert!(reason.contains("GPU backend"), "{reason}"),
            RendererKind::Cpu => assert!(reason.contains("software backend"), "{reason}"),
        }
    }
}
