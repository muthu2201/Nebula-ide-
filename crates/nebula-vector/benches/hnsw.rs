//! HNSW build and query benchmarks.
//!
//! The point of the index is that query time grows logarithmically while a
//! brute-force scan grows linearly; `search` vs `search_exact` at increasing
//! corpus sizes is what demonstrates that.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nebula_vector::{Embedder, HashingEmbedder, Hnsw, HnswConfig, Metric};
use std::hint::black_box;

fn vectors(count: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((v >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
    };
    (0..count).map(|_| (0..dim).map(|_| next()).collect()).collect()
}

fn build_index(count: usize, dim: usize) -> (Hnsw, Vec<Vec<f32>>) {
    let data = vectors(count, dim, 7);
    let mut index = Hnsw::new(HnswConfig::new(dim).metric(Metric::Cosine));
    for (i, v) in data.iter().enumerate() {
        index.insert(i as u64, v).unwrap();
    }
    (index, data)
}

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_build");
    group.sample_size(10);
    for count in [1_000usize, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| black_box(build_index(count, 384).0.len()));
        });
    }
    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_k10");
    for count in [1_000usize, 10_000, 50_000] {
        let (index, data) = build_index(count, 384);
        group.bench_with_input(
            BenchmarkId::new("hnsw", count),
            &(&index, &data),
            |b, (index, data)| {
                let mut i = 0usize;
                b.iter(|| {
                    i = (i + 1) % data.len();
                    black_box(index.search(black_box(&data[i]), 10).unwrap())
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("exhaustive", count),
            &(&index, &data),
            |b, (index, data)| {
                let mut i = 0usize;
                b.iter(|| {
                    i = (i + 1) % data.len();
                    black_box(index.search_exact(black_box(&data[i]), 10).unwrap())
                });
            },
        );
    }
    group.finish();
}

fn bench_embed(c: &mut Criterion) {
    let embedder = HashingEmbedder::new(384);
    let snippet = "pub fn compute_retry_policy(attempts: u32, backoff: Duration) -> RetryPolicy {\n\
                   \x20   RetryPolicy { max_attempts: attempts, backoff }\n}";
    c.bench_function("embed_code_snippet", |b| {
        b.iter(|| black_box(embedder.embed(black_box(snippet)).unwrap()));
    });
}

criterion_group!(benches, bench_build, bench_search, bench_embed);
criterion_main!(benches);
