//! Parsing and highlighting benchmarks.
//!
//! The comparison that matters is `incremental_keystroke` against `full_parse`:
//! the whole design of this layer rests on the first being independent of file
//! size while the second is linear in it.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nebula_core::{Edit, Range, TextBuffer, Transaction};
use nebula_syntax::{GrammarRegistry, Highlighter, SyntaxTree, symbols};
use std::hint::black_box;

fn rust_source(functions: usize) -> String {
    let mut out = String::with_capacity(functions * 220);
    for i in 0..functions {
        out.push_str(&format!(
            "/// Documentation for handler {i}.\n\
             pub fn handler_{i}(input: &str, count: u32) -> Result<String, Error> {{\n    \
             let parsed = parse_input(input)?;\n    \
             if parsed.is_empty() {{\n        \
             return Err(Error::Empty);\n    \
             }}\n    \
             Ok(format!(\"{{}}-{{}}\", parsed, count))\n\
             }}\n\n"
        ));
    }
    out
}

fn bench_full_parse(c: &mut Criterion) {
    let registry = GrammarRegistry::new();
    let grammar = registry.get("rust").unwrap();

    let mut group = c.benchmark_group("full_parse");
    for functions in [100usize, 1_000, 5_000] {
        let source = rust_source(functions);
        let buffer = TextBuffer::from_str(&source);
        group.throughput(criterion::Throughput::Bytes(source.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(functions), &buffer, |b, buffer| {
            b.iter(|| black_box(SyntaxTree::parse(grammar.clone(), black_box(buffer), 0).unwrap()));
        });
    }
    group.finish();
}

fn bench_incremental_keystroke(c: &mut Criterion) {
    let registry = GrammarRegistry::new();
    let grammar = registry.get("rust").unwrap();

    let mut group = c.benchmark_group("incremental_keystroke");
    for functions in [100usize, 1_000, 5_000] {
        let source = rust_source(functions);
        group.bench_with_input(BenchmarkId::from_parameter(functions), &source, |b, source| {
            b.iter_batched(
                || {
                    let before = TextBuffer::from_str(source);
                    let tree = SyntaxTree::parse(grammar.clone(), &before, 0).unwrap();
                    // Type into the middle of the file, the realistic case.
                    let offset = before.len_chars() / 2;
                    let transaction = Transaction::single(Edit::insert(offset, "x"));
                    let mut after = before.clone();
                    let result = transaction.apply(&mut after).unwrap();
                    (tree, before, after, transaction, result)
                },
                |(mut tree, before, after, transaction, result)| {
                    tree.apply(&before, &after, &transaction, &result, 1).unwrap();
                    black_box(tree.version())
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_highlight(c: &mut Criterion) {
    let registry = GrammarRegistry::new();
    let grammar = registry.get("rust").unwrap();
    let source = rust_source(5_000);
    let buffer = TextBuffer::from_str(&source);
    let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();

    let mut group = c.benchmark_group("highlight");
    // A 60-line viewport: what actually runs on every frame.
    group.bench_function("viewport_60_lines", |b| {
        let end = buffer.line_start(60.min(buffer.len_lines() - 1)).unwrap();
        let range = Range::new(0, end);
        let mut highlighter = Highlighter::new();
        b.iter(|| black_box(highlighter.highlight_range(&tree, &buffer, black_box(range)).unwrap()));
    });
    // Whole document: what a naive implementation would do per frame.
    group.bench_function("whole_document", |b| {
        let mut highlighter = Highlighter::new();
        b.iter(|| black_box(highlighter.highlight(&tree, &buffer).unwrap()));
    });
    group.finish();
}

fn bench_symbols(c: &mut Criterion) {
    let registry = GrammarRegistry::new();
    let grammar = registry.get("rust").unwrap();
    let source = rust_source(2_000);
    let buffer = TextBuffer::from_str(&source);
    let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();

    c.bench_function("symbol_extraction_2000_fns", |b| {
        b.iter(|| black_box(symbols::symbols(&tree, &buffer).unwrap()));
    });
}

criterion_group!(
    benches,
    bench_full_parse,
    bench_incremental_keystroke,
    bench_highlight,
    bench_symbols
);
criterion_main!(benches);
