//! Rasterisation benchmarks for the software backend.
//!
//! The CPU path is the one with a budget worth watching: it is what runs where
//! there is no GPU, and it is the one that degrades first as a frame grows.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nebula_render::backend::{Renderer, Surface};
use nebula_render::{Color, CpuRenderer, Point, Quad, Rect, Scene, TextRun};
use std::hint::black_box;

/// A frame shaped like a real editor window.
fn editor_frame(lines: usize) -> Scene {
    let mut scene = Scene::new(1280.0, 800.0, 1.0).background(Color::rgb(24, 24, 32));
    scene.quad(Quad::new(Rect::new(0.0, 0.0, 60.0, 800.0), Color::rgb(30, 30, 40)));

    for line in 0..lines {
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
    scene
}

fn bench_quads(c: &mut Criterion) {
    let mut group = c.benchmark_group("cpu_quads");
    for count in [100usize, 1_000, 10_000] {
        let mut scene = Scene::new(1280.0, 800.0, 1.0).background(Color::BLACK);
        for i in 0..count {
            let x = (i % 128) as f32 * 10.0;
            let y = (i / 128) as f32 * 10.0;
            scene.quad(Quad::new(Rect::new(x, y, 8.0, 8.0), Color::rgb(200, 100, 50)));
        }
        let mut renderer = CpuRenderer::new(Surface::new(1280, 800)).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(count), &scene, |b, scene| {
            b.iter(|| black_box(renderer.render(black_box(scene)).unwrap().pixel_count()));
        });
    }
    group.finish();
}

fn bench_text(c: &mut Criterion) {
    let mut group = c.benchmark_group("cpu_editor_frame");
    for lines in [10usize, 50] {
        let scene = editor_frame(lines);
        let mut renderer = CpuRenderer::new(Surface::new(1280, 800)).unwrap();
        // Warm the glyph cache, as it is after the first frame.
        renderer.render(&scene).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(lines), &scene, |b, scene| {
            b.iter(|| black_box(renderer.render(black_box(scene)).unwrap().pixel_count()));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_quads, bench_text);
criterion_main!(benches);
