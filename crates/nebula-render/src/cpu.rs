//! The software backend, built on `tiny-skia`.
//!
//! This is the path that runs when there is no usable GPU. It draws the same
//! [`Scene`] the GPU backend does, so nothing is missing from the interface —
//! only the frame time is worse.
//!
//! It is also the backend the tests use, because a software rasteriser produces
//! the same pixels on every machine. That makes rendering assertions
//! deterministic in a way a GPU comparison across drivers never is.

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Transform};

use crate::backend::{Framebuffer, Renderer, RendererKind, Surface};
use crate::scene::{Color, Primitive, Rect, Scene};
use crate::text::FontSystem;
use crate::{RenderError, Result};

/// A CPU rasteriser.
pub struct CpuRenderer {
    pixmap: Pixmap,
    surface: Surface,
    fonts: FontSystem,
}

impl CpuRenderer {
    /// A renderer targeting `surface`.
    pub fn new(surface: Surface) -> Result<Self> {
        let pixmap = Pixmap::new(surface.width, surface.height).ok_or(
            RenderError::InvalidSize { width: surface.width, height: surface.height },
        )?;
        Ok(Self { pixmap, surface, fonts: FontSystem::new() })
    }
}

impl Renderer for CpuRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Cpu
    }

    fn resize(&mut self, surface: Surface) -> Result<()> {
        if surface == self.surface {
            return Ok(());
        }
        self.pixmap = Pixmap::new(surface.width, surface.height).ok_or(
            RenderError::InvalidSize { width: surface.width, height: surface.height },
        )?;
        self.surface = surface;
        Ok(())
    }

    fn surface(&self) -> Surface {
        self.surface
    }

    fn render(&mut self, scene: &Scene) -> Result<Framebuffer> {
        if !scene.clips_balanced() {
            return Err(RenderError::Frame(
                "the scene has unbalanced clip regions".to_string(),
            ));
        }

        let (width, height) = scene.device_size();
        if width != self.surface.width || height != self.surface.height {
            self.resize(Surface::new(width, height))?;
        }

        self.pixmap.fill(to_skia_color(scene.background));

        let scale = scene.scale_factor;
        let transform = Transform::from_scale(scale, scale);

        // A stack of clips, intersected as they nest — a nested clip can only
        // narrow, never widen, which is what a caller expects.
        let mut clips: Vec<Rect> = Vec::new();

        for primitive in &scene.primitives {
            match primitive {
                Primitive::PushClip(rect) => {
                    let effective = match clips.last() {
                        Some(current) => current.intersection(rect).unwrap_or(Rect::default()),
                        None => *rect,
                    };
                    clips.push(effective);
                }
                Primitive::PopClip => {
                    clips.pop();
                }
                Primitive::Quad(quad) => {
                    // Clip in scene coordinates before rasterising, which is
                    // cheaper than masking and produces identical results for
                    // axis-aligned rectangles.
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

                    let mut paint = Paint::default();
                    paint.set_color(to_skia_color(quad.color));
                    paint.anti_alias = quad.corner_radius > 0.0;

                    if quad.corner_radius > 0.0 {
                        if let Some(path) = rounded_rect_path(rect, quad.corner_radius) {
                            self.pixmap.fill_path(
                                &path,
                                &paint,
                                FillRule::Winding,
                                transform,
                                None,
                            );
                        }
                    } else if let Some(sk_rect) =
                        SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height)
                    {
                        self.pixmap.fill_rect(sk_rect, &paint, transform, None);
                    }
                }
                Primitive::Text(run) => {
                    let clip = clips.last().copied();
                    self.fonts.draw_run(&mut self.pixmap, run, scale, clip)?;
                }
            }
        }

        Ok(Framebuffer {
            width: self.surface.width,
            height: self.surface.height,
            pixels: self.pixmap.data().to_vec(),
        })
    }

    fn describe(&self) -> String {
        format!(
            "tiny-skia software rasteriser, {}x{}",
            self.surface.width, self.surface.height
        )
    }
}

fn to_skia_color(color: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(color.r, color.g, color.b, color.a)
}

/// Build a rounded-rectangle path.
fn rounded_rect_path(rect: Rect, radius: f32) -> Option<tiny_skia::Path> {
    // A radius larger than half the shorter side would produce a self-
    // intersecting path.
    let radius = radius.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
    let mut builder = PathBuilder::new();

    let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
    builder.move_to(x + radius, y);
    builder.line_to(x + w - radius, y);
    builder.quad_to(x + w, y, x + w, y + radius);
    builder.line_to(x + w, y + h - radius);
    builder.quad_to(x + w, y + h, x + w - radius, y + h);
    builder.line_to(x + radius, y + h);
    builder.quad_to(x, y + h, x, y + h - radius);
    builder.line_to(x, y + radius);
    builder.quad_to(x, y, x + radius, y);
    builder.close();

    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Point, Quad, TextRun};

    fn render(scene: &Scene) -> Framebuffer {
        let (width, height) = scene.device_size();
        let mut renderer = CpuRenderer::new(Surface::new(width, height)).unwrap();
        renderer.render(scene).unwrap()
    }

    #[test]
    fn the_background_fills_the_frame() {
        let scene = Scene::new(16.0, 16.0, 1.0).background(Color::rgb(20, 40, 60));
        let frame = render(&scene);

        assert_eq!(frame.width, 16);
        assert_eq!(frame.height, 16);
        for (x, y) in [(0, 0), (15, 15), (8, 8)] {
            assert_eq!(frame.pixel(x, y), Some((20, 40, 60, 255)), "at ({x}, {y})");
        }
    }

    #[test]
    fn a_quad_is_drawn_where_it_was_asked_for() {
        let mut scene = Scene::new(32.0, 32.0, 1.0).background(Color::BLACK);
        scene.quad(Quad::new(Rect::new(8.0, 8.0, 16.0, 16.0), Color::rgb(255, 0, 0)));
        let frame = render(&scene);

        assert_eq!(frame.pixel(16, 16), Some((255, 0, 0, 255)), "inside the quad");
        assert_eq!(frame.pixel(8, 8), Some((255, 0, 0, 255)), "the top-left corner");
        assert_eq!(frame.pixel(23, 23), Some((255, 0, 0, 255)), "the bottom-right corner");

        assert_eq!(frame.pixel(4, 4), Some((0, 0, 0, 255)), "outside the quad");
        assert_eq!(frame.pixel(24, 24), Some((0, 0, 0, 255)), "just past the far edge");
    }

    #[test]
    fn quads_paint_in_order() {
        let mut scene = Scene::new(16.0, 16.0, 1.0).background(Color::BLACK);
        scene
            .quad(Quad::new(Rect::new(0.0, 0.0, 16.0, 16.0), Color::rgb(255, 0, 0)))
            .quad(Quad::new(Rect::new(0.0, 0.0, 16.0, 16.0), Color::rgb(0, 255, 0)));

        assert_eq!(render(&scene).pixel(8, 8), Some((0, 255, 0, 255)), "the later quad wins");
    }

    #[test]
    fn a_translucent_quad_blends_with_what_is_beneath() {
        let mut scene = Scene::new(16.0, 16.0, 1.0).background(Color::BLACK);
        scene.quad(Quad::new(Rect::new(0.0, 0.0, 16.0, 16.0), Color::rgba(255, 0, 0, 128)));

        let (r, g, b, a) = render(&scene).pixel(8, 8).unwrap();
        assert!(r > 100 && r < 160, "expected a half-blended red, got {r}");
        assert_eq!((g, b), (0, 0));
        assert_eq!(a, 255);
    }

    #[test]
    fn a_clip_confines_drawing_to_its_rectangle() {
        let mut scene = Scene::new(32.0, 32.0, 1.0).background(Color::BLACK);
        scene
            .push_clip(Rect::new(0.0, 0.0, 16.0, 16.0))
            .quad(Quad::new(Rect::new(0.0, 0.0, 32.0, 32.0), Color::rgb(255, 0, 0)))
            .pop_clip();
        let frame = render(&scene);

        assert_eq!(frame.pixel(8, 8), Some((255, 0, 0, 255)), "inside the clip");
        assert_eq!(frame.pixel(24, 24), Some((0, 0, 0, 255)), "outside the clip");
        assert_eq!(frame.pixel(20, 8), Some((0, 0, 0, 255)), "past the clip's right edge");
    }

    #[test]
    fn nested_clips_intersect_rather_than_replace() {
        let mut scene = Scene::new(32.0, 32.0, 1.0).background(Color::BLACK);
        scene
            .push_clip(Rect::new(0.0, 0.0, 16.0, 16.0))
            // A nested clip that is wider must not widen the visible region.
            .push_clip(Rect::new(0.0, 0.0, 32.0, 8.0))
            .quad(Quad::new(Rect::new(0.0, 0.0, 32.0, 32.0), Color::rgb(255, 0, 0)))
            .pop_clip()
            .pop_clip();
        let frame = render(&scene);

        assert_eq!(frame.pixel(4, 4), Some((255, 0, 0, 255)), "inside both clips");
        assert_eq!(frame.pixel(20, 4), Some((0, 0, 0, 255)), "outside the outer clip");
        assert_eq!(frame.pixel(4, 12), Some((0, 0, 0, 255)), "outside the inner clip");
    }

    #[test]
    fn popping_a_clip_restores_the_previous_one() {
        let mut scene = Scene::new(32.0, 32.0, 1.0).background(Color::BLACK);
        scene
            .push_clip(Rect::new(0.0, 0.0, 8.0, 8.0))
            .pop_clip()
            .quad(Quad::new(Rect::new(0.0, 0.0, 32.0, 32.0), Color::rgb(0, 0, 255)));

        assert_eq!(
            render(&scene).pixel(24, 24),
            Some((0, 0, 255, 255)),
            "drawing after the pop must not still be clipped"
        );
    }

    #[test]
    fn an_unbalanced_scene_is_refused_rather_than_drawn_differently() {
        let mut scene = Scene::new(16.0, 16.0, 1.0);
        scene.push_clip(Rect::new(0.0, 0.0, 8.0, 8.0));

        let mut renderer = CpuRenderer::new(Surface::new(16, 16)).unwrap();
        assert!(renderer.render(&scene).is_err());
    }

    #[test]
    fn the_scale_factor_multiplies_the_output() {
        let mut scene = Scene::new(16.0, 16.0, 2.0).background(Color::BLACK);
        scene.quad(Quad::new(Rect::new(0.0, 0.0, 8.0, 8.0), Color::rgb(255, 0, 0)));
        let frame = render(&scene);

        assert_eq!((frame.width, frame.height), (32, 32));
        // The 8x8 logical quad covers 16x16 device pixels.
        assert_eq!(frame.pixel(15, 15), Some((255, 0, 0, 255)));
        assert_eq!(frame.pixel(17, 17), Some((0, 0, 0, 255)));
    }

    #[test]
    fn rendering_is_deterministic() {
        // This is why the tests use the software backend: the same scene must
        // produce the same bytes on every machine.
        let mut scene = Scene::new(64.0, 64.0, 1.0).background(Color::rgb(10, 10, 10));
        scene
            .quad(Quad::rounded(Rect::new(4.0, 4.0, 32.0, 20.0), Color::rgb(200, 100, 50), 4.0))
            .text(TextRun::new(Point::new(6.0, 20.0), "Nebula", Color::WHITE, 12.0));

        assert_eq!(render(&scene).pixels, render(&scene).pixels);
    }

    #[test]
    fn text_marks_the_frame() {
        let mut scene = Scene::new(64.0, 24.0, 1.0).background(Color::BLACK);
        scene.text(TextRun::new(Point::new(2.0, 16.0), "Hello", Color::WHITE, 14.0));
        let frame = render(&scene);

        let lit = frame.pixels.chunks_exact(4).filter(|p| p[0] > 40).count();
        assert!(lit > 20, "expected glyph coverage, found {lit} lit pixels");
    }

    #[test]
    fn text_is_clipped_like_everything_else() {
        let mut unclipped = Scene::new(64.0, 24.0, 1.0).background(Color::BLACK);
        unclipped.text(TextRun::new(Point::new(2.0, 16.0), "Hello world", Color::WHITE, 14.0));

        let mut clipped = Scene::new(64.0, 24.0, 1.0).background(Color::BLACK);
        clipped
            .push_clip(Rect::new(0.0, 0.0, 16.0, 24.0))
            .text(TextRun::new(Point::new(2.0, 16.0), "Hello world", Color::WHITE, 14.0))
            .pop_clip();

        let lit = |frame: &Framebuffer| {
            frame.pixels.chunks_exact(4).filter(|p| p[0] > 40).count()
        };
        assert!(
            lit(&render(&clipped)) < lit(&render(&unclipped)),
            "a clip must reduce the text that is drawn"
        );
    }

    #[test]
    fn a_rounded_quad_leaves_its_corners_unpainted() {
        let mut scene = Scene::new(32.0, 32.0, 1.0).background(Color::BLACK);
        scene.quad(Quad::rounded(Rect::new(0.0, 0.0, 32.0, 32.0), Color::rgb(255, 0, 0), 12.0));
        let frame = render(&scene);

        assert_eq!(frame.pixel(16, 16), Some((255, 0, 0, 255)), "the middle is filled");
        let (corner_r, ..) = frame.pixel(0, 0).unwrap();
        assert!(corner_r < 128, "the corner should be mostly background, got r={corner_r}");
    }

    #[test]
    fn an_oversized_corner_radius_does_not_produce_a_broken_path() {
        let mut scene = Scene::new(16.0, 16.0, 1.0).background(Color::BLACK);
        scene.quad(Quad::rounded(Rect::new(0.0, 0.0, 8.0, 8.0), Color::rgb(255, 0, 0), 1000.0));

        // Clamped to half the shorter side, so the result is a circle rather
        // than a self-intersecting path.
        let frame = render(&scene);
        assert_eq!(frame.pixel(4, 4), Some((255, 0, 0, 255)));
    }

    #[test]
    fn resizing_reallocates_the_target() {
        let mut renderer = CpuRenderer::new(Surface::new(16, 16)).unwrap();
        assert_eq!(renderer.surface(), Surface::new(16, 16));

        renderer.resize(Surface::new(64, 48)).unwrap();
        assert_eq!(renderer.surface(), Surface::new(64, 48));

        let frame = renderer.render(&Scene::new(64.0, 48.0, 1.0)).unwrap();
        assert_eq!((frame.width, frame.height), (64, 48));
    }

    #[test]
    fn a_zero_sized_surface_is_refused() {
        assert!(matches!(
            CpuRenderer::new(Surface::new(0, 100)),
            Err(RenderError::InvalidSize { .. })
        ));
    }

    #[test]
    fn a_scene_larger_than_the_surface_resizes_it() {
        let mut renderer = CpuRenderer::new(Surface::new(16, 16)).unwrap();
        let frame = renderer.render(&Scene::new(64.0, 64.0, 1.0)).unwrap();
        assert_eq!((frame.width, frame.height), (64, 64));
    }

    #[test]
    fn a_frame_with_many_primitives_stays_within_the_cpu_budget() {
        // A realistic editor frame: a background, gutter, selection, and one
        // text run per visible line.
        let mut scene = Scene::new(1280.0, 800.0, 1.0).background(Color::rgb(24, 24, 32));
        scene.quad(Quad::new(Rect::new(0.0, 0.0, 60.0, 800.0), Color::rgb(30, 30, 40)));

        for line in 0..50 {
            let y = line as f32 * 16.0;
            scene
                .text(TextRun::new(
                    Point::new(8.0, y + 12.0),
                    format!("{:>4}", line + 1),
                    Color::rgb(90, 90, 110),
                    12.0,
                ))
                .text(TextRun::new(
                    Point::new(64.0, y + 12.0),
                    "    let result = compute(input, options)?;",
                    Color::rgb(220, 220, 230),
                    13.0,
                ));
        }

        let mut renderer = CpuRenderer::new(Surface::new(1280, 800)).unwrap();
        // Warm the glyph cache, as it would be after the first frame.
        renderer.render(&scene).unwrap();

        let started = std::time::Instant::now();
        renderer.render(&scene).unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "a warm software frame took {elapsed:?}, which is far outside any usable budget"
        );
    }
}
