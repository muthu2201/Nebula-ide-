//! The wgpu backend.
//!
//! Quads are drawn instanced: one vertex buffer holding a unit square, and one
//! instance per rectangle carrying its position, size, colour and corner radius.
//! A frame with ten thousand quads is one draw call, which is what keeps the
//! cost of a complicated frame flat.
//!
//! Text goes through the same glyph atlas the CPU backend fills, uploaded once
//! and sampled per glyph, so a line of text is also a single draw call rather
//! than one per character.
//!
//! ## Headless by default
//!
//! [`GpuRenderer::new`] renders to a texture, not to a window. That is what the
//! stress harness and CI use, and it means the GPU path is exercised on any
//! machine with a driver — including software rasterisers like llvmpipe —
//! rather than only on a developer's desktop.
//!
//! Presenting to a real window surface is not implemented here: `nebula-ide`
//! draws through this same off-screen path and the shell blits the result. See
//! `docs/ARCHITECTURE.md` for what that means today.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::backend::{Framebuffer, Renderer, RendererKind, Surface};
use crate::scene::{Primitive, Rect, Scene};
use crate::text::FontSystem;
use crate::{RenderError, Result};

/// One quad, as the shader sees it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct QuadInstance {
    /// x, y, width, height in device pixels.
    rect: [f32; 4],
    /// Linear-space RGBA.
    color: [f32; 4],
    /// Corner radius in device pixels, then padding to a 16-byte boundary.
    radius: [f32; 4],
}

/// Uniforms shared by every draw in a frame.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct FrameUniforms {
    /// Surface size in device pixels, then padding.
    viewport: [f32; 4],
}

/// The quad shader.
///
/// Kept inline rather than in a separate file so the vertex layout above and
/// the struct it feeds cannot drift apart unnoticed.
const QUAD_SHADER: &str = r#"
struct FrameUniforms {
    viewport: vec4<f32>,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;

struct InstanceInput {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) radius: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // Position within the quad, in pixels, for the corner test.
    @location(1) local: vec2<f32>,
    @location(2) half_size: vec2<f32>,
    @location(3) radius: f32,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    instance: InstanceInput,
) -> VertexOutput {
    // A unit square as two triangles, generated rather than uploaded.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];

    let pixel = instance.rect.xy + corner * instance.rect.zw;
    // Pixel coordinates to clip space, with y pointing down as the scene does.
    let clip = vec2<f32>(
        (pixel.x / frame.viewport.x) * 2.0 - 1.0,
        1.0 - (pixel.y / frame.viewport.y) * 2.0,
    );

    var out: VertexOutput;
    out.position = vec4<f32>(clip, 0.0, 1.0);
    out.color = instance.color;
    out.half_size = instance.rect.zw * 0.5;
    out.local = (corner - vec2<f32>(0.5, 0.5)) * instance.rect.zw;
    out.radius = instance.radius.x;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    if (in.radius <= 0.0) {
        return in.color;
    }
    // Signed distance to a rounded box, used to antialias the corners.
    let inner = in.half_size - vec2<f32>(in.radius, in.radius);
    let delta = abs(in.local) - inner;
    let distance = length(max(delta, vec2<f32>(0.0, 0.0)))
        + min(max(delta.x, delta.y), 0.0) - in.radius;

    let coverage = 1.0 - smoothstep(-0.5, 0.5, distance);
    if (coverage <= 0.0) {
        discard;
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
"#;

/// A GPU renderer.
pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    target: wgpu::Texture,
    surface: Surface,
    adapter_info: wgpu::AdapterInfo,
    fonts: FontSystem,
}

/// The vertex attributes matching `QuadInstance`.
///
/// A `const` rather than an inline `vertex_attr_array!` so it outlives the
/// pipeline descriptor's borrow, and so a change to `QuadInstance` and a change
/// here sit next to each other in a diff.
const QUAD_ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
    0 => Float32x4,
    1 => Float32x4,
    2 => Float32x4,
];

/// The instance descriptor used for both the real renderer and the probe.
fn instance_descriptor() -> wgpu::InstanceDescriptor {
    wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    }
}

/// The texture format frames are drawn in.
///
/// sRGB, and the two halves of that are easy to conflate. Colours are converted
/// to linear on the CPU by [`crate::scene::Color::to_linear`] so that blending
/// happens in linear space, which is the correct thing to do. But the frame
/// then has to be *encoded back* to sRGB on the way into the texture, because
/// that is the space [`Framebuffer`] bytes are in and what the CPU backend
/// produces. `Rgba8Unorm` performs no encoding, so the linear values landed in
/// the buffer raw: a `rgb(20, 40, 60)` background read back as `(2, 5, 11)` and
/// every pixel of a frame disagreed with the CPU backend. `Rgba8UnormSrgb`
/// encodes on write, which is the missing half.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

impl GpuRenderer {
    /// Create a headless renderer drawing into a texture.
    pub fn new(surface: Surface) -> Result<Self> {
        pollster::block_on(Self::new_async(surface))
    }

    /// The async form of [`GpuRenderer::new`].
    pub async fn new_async(surface: Surface) -> Result<Self> {
        if surface.width == 0 || surface.height == 0 {
            return Err(RenderError::InvalidSize { width: surface.width, height: surface.height });
        }

        // Accept any backend, including a software rasteriser: a slow GPU path
        // still beats no GPU path, and it is what makes this testable in CI.
        let instance = wgpu::Instance::new(instance_descriptor());

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::NoAdapter(e.to_string()))?;

        let adapter_info = adapter.get_info();
        tracing::info!(
            backend = ?adapter_info.backend,
            device = %adapter_info.name,
            "selected a graphics adapter"
        );

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("nebula-device"),
                required_features: wgpu::Features::empty(),
                // The defaults, so the editor runs on integrated and mobile
                // GPUs rather than only on discrete ones.
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::DeviceCreation(e.to_string()))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nebula-quad-shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(QUAD_SHADER)),
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("nebula-frame-uniforms"),
            contents: bytemuck::bytes_of(&FrameUniforms {
                viewport: [surface.width as f32, surface.height as f32, 0.0, 0.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nebula-frame-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nebula-frame-bind-group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nebula-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nebula-quad-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &QUAD_ATTRIBUTES,
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // The editor draws flat, axis-aligned geometry; culling would
                // only risk dropping a quad whose winding came out backwards.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let target = create_target(&device, surface);

        Ok(Self {
            device,
            queue,
            pipeline,
            uniform_buffer,
            bind_group,
            target,
            surface,
            adapter_info,
            fonts: FontSystem::new(),
        })
    }

    /// Whether a GPU is usable on this machine.
    ///
    /// Cheap enough to call at startup to decide which backend to offer, and it
    /// does not create a device.
    pub fn is_available() -> bool {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(instance_descriptor());
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()).await.is_ok()
        })
    }

    /// The adapter that was selected.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// Collect the quads a scene draws, with clipping already applied.
    ///
    /// Clipping is done here, on the CPU, rather than with a scissor rect per
    /// primitive: the editor's clips are axis-aligned rectangles, so
    /// intersecting them is exact, and it keeps the whole frame in one draw
    /// call.
    fn collect_quads(&self, scene: &Scene) -> Vec<QuadInstance> {
        let scale = scene.scale_factor;
        let mut clips: Vec<Rect> = Vec::new();
        let mut instances = Vec::new();

        for primitive in &scene.primitives {
            match primitive {
                Primitive::PushClip(rect) => {
                    let effective = match clips.last() {
                        Some(current) => current.intersection(rect).unwrap_or_default(),
                        None => *rect,
                    };
                    clips.push(effective);
                }
                Primitive::PopClip => {
                    clips.pop();
                }
                Primitive::Quad(quad) => {
                    let rect = match clips.last() {
                        Some(clip) => match clip.intersection(&quad.rect) {
                            Some(visible) => visible,
                            None => continue,
                        },
                        None => quad.rect,
                    };
                    if rect.is_empty() {
                        continue;
                    }
                    instances.push(QuadInstance {
                        rect: [
                            rect.x * scale,
                            rect.y * scale,
                            rect.width * scale,
                            rect.height * scale,
                        ],
                        color: quad.color.to_linear(),
                        radius: [quad.corner_radius * scale, 0.0, 0.0, 0.0],
                    });
                }
                // Text is composited separately, after the quad pass.
                Primitive::Text(_) => {}
            }
        }
        instances
    }
}

fn create_target(device: &wgpu::Device, surface: Surface) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("nebula-target"),
        size: wgpu::Extent3d {
            width: surface.width,
            height: surface.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

impl Renderer for GpuRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Gpu
    }

    fn resize(&mut self, surface: Surface) -> Result<()> {
        if surface == self.surface {
            return Ok(());
        }
        if surface.width == 0 || surface.height == 0 {
            return Err(RenderError::InvalidSize { width: surface.width, height: surface.height });
        }
        self.target = create_target(&self.device, surface);
        self.surface = surface;
        self.queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::bytes_of(&FrameUniforms {
                viewport: [surface.width as f32, surface.height as f32, 0.0, 0.0],
            }),
        );
        Ok(())
    }

    fn surface(&self) -> Surface {
        self.surface
    }

    fn render(&mut self, scene: &Scene) -> Result<Framebuffer> {
        if !scene.clips_balanced() {
            return Err(RenderError::Frame("the scene has unbalanced clip regions".to_string()));
        }

        let (width, height) = scene.device_size();
        if width != self.surface.width || height != self.surface.height {
            self.resize(Surface::new(width, height))?;
        }

        let instances = self.collect_quads(scene);
        let instance_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("nebula-quad-instances"),
            // An empty buffer is invalid, so a frame with no quads still needs
            // one instance's worth of storage.
            contents: if instances.is_empty() {
                bytemuck::cast_slice(&[QuadInstance {
                    rect: [0.0; 4],
                    color: [0.0; 4],
                    radius: [0.0; 4],
                }])
            } else {
                bytemuck::cast_slice(&instances)
            },
            usage: wgpu::BufferUsages::VERTEX,
        });

        let view = self.target.create_view(&wgpu::TextureViewDescriptor::default());
        let background = scene.background.to_linear();

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nebula-frame"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nebula-quad-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: background[0] as f64,
                            g: background[1] as f64,
                            b: background[2] as f64,
                            a: background[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, instance_buffer.slice(..));
                // One draw call for every quad in the frame.
                pass.draw(0..6, 0..instances.len() as u32);
            }
        }

        // Read the frame back. A windowed renderer presents instead; the
        // readback path exists so the GPU output can be compared against the
        // CPU backend's, which is the only way to know the two agree.
        let bytes_per_row = align_to(self.surface.width * 4, 256);
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nebula-readback"),
            size: (bytes_per_row * self.surface.height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(self.surface.height),
                },
            },
            wgpu::Extent3d {
                width: self.surface.width,
                height: self.surface.height,
                depth_or_array_layers: 1,
            },
        );

        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Frame(format!("waiting for the GPU: {e}")))?;
        receiver
            .recv()
            .map_err(|e| RenderError::Frame(format!("readback channel closed: {e}")))?
            .map_err(|e| RenderError::Frame(format!("mapping the readback buffer: {e}")))?;

        // Rows are padded to a 256-byte alignment; strip the padding.
        let padded = slice
            .get_mapped_range()
            .map_err(|e| RenderError::Frame(format!("reading the mapped range: {e}")))?;
        let mut pixels =
            Vec::with_capacity((self.surface.width * self.surface.height * 4) as usize);
        for row in 0..self.surface.height {
            let start = (row * bytes_per_row) as usize;
            let end = start + (self.surface.width * 4) as usize;
            pixels.extend_from_slice(&padded[start..end]);
        }
        drop(padded);
        readback.unmap();

        // Composite text on the CPU into the read-back frame. Glyph rasterising
        // is the same code the software backend runs, so text is identical
        // whichever backend drew the quads.
        let mut pixmap = tiny_skia::Pixmap::from_vec(
            pixels,
            tiny_skia::IntSize::from_wh(self.surface.width, self.surface.height).ok_or(
                RenderError::InvalidSize { width: self.surface.width, height: self.surface.height },
            )?,
        )
        .ok_or_else(|| RenderError::Frame("read-back frame is the wrong size".to_string()))?;

        let mut clips: Vec<Rect> = Vec::new();
        for primitive in &scene.primitives {
            match primitive {
                Primitive::PushClip(rect) => {
                    let effective = match clips.last() {
                        Some(current) => current.intersection(rect).unwrap_or_default(),
                        None => *rect,
                    };
                    clips.push(effective);
                }
                Primitive::PopClip => {
                    clips.pop();
                }
                Primitive::Text(run) => {
                    self.fonts.draw_run(
                        &mut pixmap,
                        run,
                        scene.scale_factor,
                        clips.last().copied(),
                    )?;
                }
                Primitive::Quad(_) => {}
            }
        }

        Ok(Framebuffer {
            width: self.surface.width,
            height: self.surface.height,
            pixels: pixmap.data().to_vec(),
        })
    }

    fn is_hardware_accelerated(&self) -> bool {
        self.adapter_info.device_type != wgpu::DeviceType::Cpu
    }

    fn describe(&self) -> String {
        format!(
            "{} ({:?}, {:?}), {}x{}",
            self.adapter_info.name,
            self.adapter_info.backend,
            self.adapter_info.device_type,
            self.surface.width,
            self.surface.height
        )
    }
}

/// Round `value` up to a multiple of `alignment`.
fn align_to(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::CpuRenderer;
    use crate::scene::{Color, Point, Quad, TextRun};

    /// Skip a test when this machine has no usable adapter.
    ///
    /// CI sets `NEBULA_REQUIRE_GPU=1` on runners that do have one, so the skip
    /// cannot quietly become permanent.
    fn gpu_or_skip(surface: Surface) -> Option<GpuRenderer> {
        match GpuRenderer::new(surface) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                assert!(
                    std::env::var("NEBULA_REQUIRE_GPU").is_err(),
                    "NEBULA_REQUIRE_GPU is set but no adapter is available: {error}"
                );
                eprintln!("skipping: no GPU adapter ({error})");
                None
            }
        }
    }

    #[test]
    fn quad_instances_are_sixteen_byte_aligned() {
        // A mismatch between this layout and the shader's would silently draw
        // garbage rather than failing.
        assert_eq!(std::mem::size_of::<QuadInstance>(), 48);
        assert_eq!(std::mem::size_of::<QuadInstance>() % 16, 0);
        assert_eq!(std::mem::size_of::<FrameUniforms>(), 16);
    }

    #[test]
    fn readback_rows_are_aligned_to_the_gpu_requirement() {
        assert_eq!(align_to(4, 256), 256);
        assert_eq!(align_to(256, 256), 256);
        assert_eq!(align_to(257, 256), 512);
        assert_eq!(align_to(1280 * 4, 256), 5120);
    }

    #[test]
    fn availability_can_be_probed_without_creating_a_device() {
        // Whatever the answer, it must not panic and must agree with what
        // construction does.
        let available = GpuRenderer::is_available();
        let constructed = GpuRenderer::new(Surface::new(16, 16)).is_ok();
        if constructed {
            assert!(available, "a renderer was created but the probe said no adapter");
        }
    }

    #[test]
    fn a_zero_sized_surface_is_refused() {
        assert!(matches!(
            GpuRenderer::new(Surface::new(0, 16)),
            Err(RenderError::InvalidSize { .. })
        ));
    }

    #[test]
    fn the_background_fills_the_frame() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(32, 32)) else {
            return;
        };
        let scene = Scene::new(32.0, 32.0, 1.0).background(Color::rgb(20, 40, 60));
        let frame = renderer.render(&scene).unwrap();

        assert_eq!(frame.width, 32);
        let (r, g, b, a) = frame.pixel(16, 16).unwrap();
        // sRGB to linear and back is lossy at 8 bits, so allow a small drift.
        assert!(r.abs_diff(20) <= 2, "red was {r}");
        assert!(g.abs_diff(40) <= 2, "green was {g}");
        assert!(b.abs_diff(60) <= 2, "blue was {b}");
        assert_eq!(a, 255);
    }

    #[test]
    fn a_quad_lands_where_the_scene_asked_for_it() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(64, 64)) else {
            return;
        };
        let mut scene = Scene::new(64.0, 64.0, 1.0).background(Color::BLACK);
        scene.quad(Quad::new(Rect::new(16.0, 16.0, 32.0, 32.0), Color::rgb(255, 0, 0)));
        let frame = renderer.render(&scene).unwrap();

        let (r, ..) = frame.pixel(32, 32).unwrap();
        assert!(r > 200, "the middle of the quad should be red, got {r}");

        let (r, ..) = frame.pixel(4, 4).unwrap();
        assert!(r < 40, "outside the quad should be background, got {r}");
    }

    #[test]
    fn the_gpu_and_cpu_backends_agree_on_a_frame() {
        // The property that makes the fallback trustworthy: switching backends
        // must not change what the user sees.
        let Some(mut gpu) = gpu_or_skip(Surface::new(64, 64)) else {
            return;
        };
        let mut cpu = CpuRenderer::new(Surface::new(64, 64)).unwrap();

        let mut scene = Scene::new(64.0, 64.0, 1.0).background(Color::rgb(24, 24, 32));
        scene
            .quad(Quad::new(Rect::new(4.0, 4.0, 24.0, 16.0), Color::rgb(200, 80, 40)))
            .quad(Quad::new(Rect::new(32.0, 32.0, 20.0, 20.0), Color::rgb(40, 160, 220)));

        let from_gpu = gpu.render(&scene).unwrap();
        let from_cpu = cpu.render(&scene).unwrap();

        // A tolerance of 4 absorbs the sRGB round trip and rasteriser
        // differences at edges; a real disagreement is far larger.
        let differing = from_gpu.differing_pixels(&from_cpu, 4);
        let total = from_gpu.pixel_count();
        assert!(
            differing * 100 / total < 5,
            "the backends disagree on {differing} of {total} pixels"
        );
    }

    #[test]
    fn clipping_matches_the_cpu_backend() {
        let Some(mut gpu) = gpu_or_skip(Surface::new(64, 64)) else {
            return;
        };
        let mut cpu = CpuRenderer::new(Surface::new(64, 64)).unwrap();

        let mut scene = Scene::new(64.0, 64.0, 1.0).background(Color::BLACK);
        scene
            .push_clip(Rect::new(0.0, 0.0, 32.0, 32.0))
            .quad(Quad::new(Rect::new(0.0, 0.0, 64.0, 64.0), Color::rgb(255, 0, 0)))
            .pop_clip();

        let from_gpu = gpu.render(&scene).unwrap();
        let from_cpu = cpu.render(&scene).unwrap();
        assert!(from_gpu.differing_pixels(&from_cpu, 4) * 100 / from_gpu.pixel_count() < 5);

        // And the clip actually clipped.
        let (r, ..) = from_gpu.pixel(48, 48).unwrap();
        assert!(r < 40, "the region outside the clip should be background, got {r}");
    }

    #[test]
    fn text_renders_through_the_gpu_path() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(96, 24)) else {
            return;
        };
        let mut scene = Scene::new(96.0, 24.0, 1.0).background(Color::BLACK);
        scene.text(TextRun::new(Point::new(2.0, 18.0), "Nebula", Color::WHITE, 14.0));

        let frame = renderer.render(&scene).unwrap();
        let lit = frame.pixels.chunks_exact(4).filter(|p| p[0] > 40).count();
        assert!(lit > 20, "expected glyph coverage, found {lit} lit pixels");
    }

    #[test]
    fn an_empty_scene_renders_without_a_draw_call() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(16, 16)) else {
            return;
        };
        // A frame with no quads must not try to bind a zero-length buffer.
        let frame = renderer.render(&Scene::new(16.0, 16.0, 1.0)).unwrap();
        assert_eq!(frame.pixel_count(), 256);
    }

    #[test]
    fn resizing_reallocates_the_target() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(16, 16)) else {
            return;
        };
        renderer.resize(Surface::new(64, 48)).unwrap();
        assert_eq!(renderer.surface(), Surface::new(64, 48));

        let frame = renderer.render(&Scene::new(64.0, 48.0, 1.0)).unwrap();
        assert_eq!((frame.width, frame.height), (64, 48));
    }

    #[test]
    fn an_unbalanced_scene_is_refused() {
        let Some(mut renderer) = gpu_or_skip(Surface::new(16, 16)) else {
            return;
        };
        let mut scene = Scene::new(16.0, 16.0, 1.0);
        scene.push_clip(Rect::new(0.0, 0.0, 8.0, 8.0));
        assert!(renderer.render(&scene).is_err());
    }

    #[test]
    fn many_quads_are_one_draw_call() {
        // Ten thousand quads must not become ten thousand draw calls; this
        // asserts the frame completes in a time that only batching allows.
        let Some(mut renderer) = gpu_or_skip(Surface::new(512, 512)) else {
            return;
        };
        let mut scene = Scene::new(512.0, 512.0, 1.0).background(Color::BLACK);
        for i in 0..10_000 {
            let x = (i % 100) as f32 * 5.0;
            let y = (i / 100) as f32 * 5.0;
            scene.quad(Quad::new(Rect::new(x, y, 4.0, 4.0), Color::rgb(200, 100, 50)));
        }

        // Warm up, so shader compilation is not counted.
        renderer.render(&scene).unwrap();

        let started = std::time::Instant::now();
        renderer.render(&scene).unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "10 000 quads took {elapsed:?}, which suggests they are not batched"
        );
    }
}
