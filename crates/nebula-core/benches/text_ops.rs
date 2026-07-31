//! Benchmarks for the operations on the keystroke-to-photon path.
//!
//! The performance budget is 8 ms from keypress to a submitted frame. Text
//! model work is only one slice of that, so these benchmarks exist to catch a
//! regression that turns an O(log n) rope operation into an O(n) scan.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nebula_core::{Document, Edit, Range, Selection, SelectionSet, TextBuffer, Transaction};
use std::hint::black_box;

fn synth_source(lines: usize) -> String {
    // Shaped like real code: mixed line lengths, indentation, identifiers.
    let mut out = String::with_capacity(lines * 48);
    for i in 0..lines {
        match i % 7 {
            0 => out.push_str(&format!("pub fn handler_{i}(input: &Request) -> Response {{\n")),
            1 => out.push_str("    let parsed = parse(input)?;\n"),
            2 => out.push_str("    if parsed.is_empty() {\n"),
            3 => out.push_str("        return Response::empty();\n"),
            4 => out.push_str("    }\n"),
            5 => out.push_str(&format!("    Response::from(parsed, {i})\n")),
            _ => out.push_str("}\n\n"),
        }
    }
    out
}

fn bench_load(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_load");
    for lines in [1_000usize, 50_000, 200_000] {
        let text = synth_source(lines);
        group.bench_with_input(BenchmarkId::from_parameter(lines), &text, |b, text| {
            b.iter(|| black_box(TextBuffer::from_str(black_box(text))));
        });
    }
    group.finish();
}

fn bench_offset_conversions(c: &mut Criterion) {
    let buffer = TextBuffer::from_str(&synth_source(200_000));
    let len = buffer.len_chars();
    let probes: Vec<usize> = (0..1000).map(|i| (i * len) / 1000).collect();

    let mut group = c.benchmark_group("offset_conversion");
    group.bench_function("offset_to_position", |b| {
        b.iter(|| {
            for &p in &probes {
                black_box(buffer.offset_to_position(black_box(p)).unwrap());
            }
        });
    });
    group.bench_function("offset_to_lsp_position_utf16", |b| {
        b.iter(|| {
            for &p in &probes {
                black_box(buffer.offset_to_lsp_position(black_box(p)).unwrap());
            }
        });
    });
    group.finish();
}

fn bench_single_keystroke(c: &mut Criterion) {
    // The headline number: one character typed into a large document, including
    // history recording and selection mapping.
    let mut group = c.benchmark_group("keystroke");
    for lines in [1_000usize, 200_000] {
        group.bench_with_input(BenchmarkId::from_parameter(lines), &lines, |b, &lines| {
            let text = synth_source(lines);
            b.iter_batched(
                || {
                    let mut doc = Document::from_str(&text);
                    let mid = doc.buffer().len_chars() / 2;
                    doc.set_caret(mid);
                    doc
                },
                |mut doc| {
                    doc.insert_at_cursors(black_box("x"), true).unwrap();
                    black_box(doc.version())
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_multi_cursor(c: &mut Criterion) {
    let text = synth_source(20_000);
    let mut group = c.benchmark_group("multi_cursor_insert");
    for cursors in [10usize, 100, 1_000] {
        group.bench_with_input(BenchmarkId::from_parameter(cursors), &cursors, |b, &cursors| {
            b.iter_batched(
                || {
                    let mut doc = Document::from_str(&text);
                    let len = doc.buffer().len_chars();
                    let sels = (0..cursors).map(|i| Selection::caret((i * len) / (cursors + 1)));
                    doc.set_selections(SelectionSet::from_iter(sels));
                    doc
                },
                |mut doc| {
                    doc.insert_at_cursors(black_box("// "), false).unwrap();
                    black_box(doc.version())
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_undo_redo(c: &mut Criterion) {
    let text = synth_source(50_000);
    c.bench_function("undo_redo_roundtrip", |b| {
        b.iter_batched(
            || {
                let mut doc = Document::from_str(&text);
                doc.set_caret(doc.buffer().len_chars() / 2);
                doc.insert_at_cursors("inserted text", false).unwrap();
                doc
            },
            |mut doc| {
                doc.undo().unwrap();
                doc.redo().unwrap();
                black_box(doc.version())
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_transaction_mapping(c: &mut Criterion) {
    // Selection mapping runs once per transaction per cursor; with a thousand
    // cursors it must not become the dominant cost.
    let transaction = Transaction::from_edits(
        (0..500).map(|i| Edit::replace(Range::new(i * 20, i * 20 + 5), "REPL")),
    )
    .unwrap();
    let selections = SelectionSet::from_iter((0..500).map(|i| Selection::caret(i * 20 + 10)));

    c.bench_function("map_selections_500", |b| {
        b.iter(|| black_box(transaction.map_selections(black_box(&selections))));
    });
}

criterion_group!(
    benches,
    bench_load,
    bench_offset_conversions,
    bench_single_keystroke,
    bench_multi_cursor,
    bench_undo_redo,
    bench_transaction_mapping
);
criterion_main!(benches);
