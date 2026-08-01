//! The project the harness works on.
//!
//! Everything here is a real, compilable, runnable program. Nothing is a stub
//! or a fragment: the harness compiles and executes each one and checks what it
//! printed, so a fixture that does not build is a harness that does not test
//! anything.
//!
//! The generated tree is also deliberately shaped like a real repository —
//! cross-file references, a `.gitignore`, a test directory, a generated file
//! large enough to be uncomfortable — because the parts of an editor that break
//! under load are the ones that assume files are small and independent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A program the harness can build and run.
#[derive(Debug, Clone)]
pub struct Program {
    /// The language, matching the editor's language identifier.
    pub language: &'static str,
    /// Where the entry point lives, relative to the project root.
    pub entry: PathBuf,
    /// How to build it, if it needs building. `None` means interpreted.
    pub build: Option<Vec<String>>,
    /// How to run it.
    pub run: Vec<String>,
    /// What running it must print.
    pub expect: &'static str,
    /// Which executable has to be on `PATH` for this program to be runnable.
    pub requires: &'static str,
}

/// The generated project.
#[derive(Debug)]
pub struct Fixture {
    /// Where it was written.
    pub root: PathBuf,
    /// The runnable programs in it.
    pub programs: Vec<Program>,
    /// How many files were written.
    pub files: usize,
    /// How many lines, in total.
    pub lines: usize,
}

/// How big the generated file is, in lines.
///
/// 200 000 lines is the blueprint's stated worst case, and the number every
/// viewport and highlighting decision was made against. Generating it here
/// means the claim is measured rather than asserted.
pub const GENERATED_LINES: usize = 200_000;

impl Fixture {
    /// Write the project into `root`.
    pub fn generate(root: &Path, generated_lines: usize) -> Result<Fixture> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("could not create {}", root.display()))?;

        let mut files = Vec::new();

        // --- Rust -----------------------------------------------------------
        files.push(("Cargo.toml", CARGO_TOML.to_string()));
        files.push(("src/main.rs", RUST_MAIN.to_string()));
        files.push(("src/geometry.rs", RUST_GEOMETRY.to_string()));
        files.push(("src/stats.rs", RUST_STATS.to_string()));

        // --- Python ---------------------------------------------------------
        files.push(("scripts/analyse.py", PYTHON_ANALYSE.to_string()));
        files.push(("scripts/util.py", PYTHON_UTIL.to_string()));

        // --- JavaScript -----------------------------------------------------
        files.push(("web/index.js", JS_INDEX.to_string()));
        files.push(("web/lib.js", JS_LIB.to_string()));

        // --- Go -------------------------------------------------------------
        files.push(("cmd/report/main.go", GO_MAIN.to_string()));

        // --- C --------------------------------------------------------------
        files.push(("native/sieve.c", C_SIEVE.to_string()));

        // --- Shell ----------------------------------------------------------
        files.push(("scripts/summary.sh", SHELL_SUMMARY.to_string()));

        // --- Data and config ------------------------------------------------
        files.push(("config/settings.json", CONFIG_JSON.to_string()));
        files.push(("config/limits.toml", CONFIG_TOML.to_string()));
        files.push(("README.md", README.to_string()));
        files.push((".gitignore", "/target\n/node_modules\n*.o\nreport.json\n".to_string()));

        // --- The uncomfortable one ------------------------------------------
        files.push(("src/generated.rs", generated_rust(generated_lines)));

        let mut total_lines = 0;
        for (relative, contents) in &files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("could not write {}", path.display()))?;
            total_lines += contents.lines().count();
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("scripts/summary.sh"),
                std::fs::Permissions::from_mode(0o755),
            )?;
        }

        Ok(Fixture {
            root: root.to_path_buf(),
            programs: programs(),
            files: files.len(),
            lines: total_lines,
        })
    }

    /// Every source file in the project, in a stable order.
    pub fn sources(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            "src/main.rs",
            "src/geometry.rs",
            "src/stats.rs",
            "scripts/analyse.py",
            "scripts/util.py",
            "web/index.js",
            "web/lib.js",
            "cmd/report/main.go",
            "native/sieve.c",
            "config/settings.json",
            "config/limits.toml",
            "README.md",
        ]
        .iter()
        .map(|p| self.root.join(p))
        .collect();
        paths.sort();
        paths
    }

    /// The generated file, which is the one that stresses the editor.
    pub fn generated(&self) -> PathBuf {
        self.root.join("src/generated.rs")
    }
}

/// Every program the harness knows how to build and run.
fn programs() -> Vec<Program> {
    vec![
        Program {
            language: "rust",
            entry: PathBuf::from("src/main.rs"),
            // `rustc` directly rather than `cargo`: the harness must not need
            // network access to fetch a registry index, and this crate has no
            // dependencies by design.
            build: Some(vec![
                "rustc".into(),
                "--edition".into(),
                "2021".into(),
                "-O".into(),
                "src/main.rs".into(),
                "-o".into(),
                "nebula-fixture-rust".into(),
            ]),
            run: vec!["./nebula-fixture-rust".into()],
            expect: "area=78.54 mean=3.00 stddev=1.41",
            requires: "rustc",
        },
        Program {
            language: "python",
            entry: PathBuf::from("scripts/analyse.py"),
            build: None,
            run: vec!["python3".into(), "scripts/analyse.py".into()],
            expect: "records=5 total=150 mean=30.0",
            requires: "python3",
        },
        Program {
            language: "javascript",
            entry: PathBuf::from("web/index.js"),
            build: None,
            run: vec!["node".into(), "web/index.js".into()],
            expect: "primes<50=15 sum=328",
            requires: "node",
        },
        Program {
            language: "go",
            entry: PathBuf::from("cmd/report/main.go"),
            build: None,
            run: vec!["go".into(), "run".into(), "cmd/report/main.go".into()],
            expect: "lines=4 words=21 longest=comprehensive",
            requires: "go",
        },
        Program {
            language: "c",
            entry: PathBuf::from("native/sieve.c"),
            build: Some(vec![
                "cc".into(),
                "-O2".into(),
                "-o".into(),
                "nebula-fixture-sieve".into(),
                "native/sieve.c".into(),
            ]),
            run: vec!["./nebula-fixture-sieve".into()],
            expect: "primes below 1000: 168",
            requires: "cc",
        },
        Program {
            language: "bash",
            entry: PathBuf::from("scripts/summary.sh"),
            build: None,
            run: vec!["sh".into(), "scripts/summary.sh".into()],
            expect: "sources=12",
            requires: "sh",
        },
    ]
}

/// A large, valid Rust file.
///
/// Every function is distinct, so tree-sitter cannot shortcut the parse, and the
/// whole thing compiles — a "large file" made of repeated identical lines does
/// not exercise the parser the way real generated code does.
fn generated_rust(lines: usize) -> String {
    let functions = (lines / 4).max(1);
    let mut out = String::with_capacity(lines * 40);
    out.push_str(
        "//! Generated. This file exists to make the editor prove its claims about\n\
         //! large files: opening it, scrolling it and typing into it must cost the\n\
         //! same per frame as a fifty-line file does.\n\n\
         #![allow(dead_code)]\n\n",
    );

    for i in 0..functions {
        out.push_str(&format!(
            "/// Computes case {i}.\n\
             pub fn case_{i}(input: u64) -> u64 {{\n\
             \x20   let scaled = input.wrapping_mul({}).wrapping_add({i});\n\
             \x20   scaled ^ (scaled >> {})\n\
             }}\n\n",
            (i % 97) as u64 + 3,
            (i % 31) + 1
        ));
    }
    out
}

const CARGO_TOML: &str = r#"[package]
name = "nebula-fixture"
version = "0.1.0"
edition = "2021"

# No dependencies on purpose: the harness has to build this without touching
# the network, on a runner that may have no registry cache at all.
[dependencies]
"#;

const RUST_MAIN: &str = r#"//! The fixture's entry point.

mod geometry;
mod stats;

use geometry::Circle;
use stats::Summary;

fn main() {
    let circle = Circle::new(5.0);
    let samples = [1.0_f64, 2.0, 3.0, 4.0, 5.0];
    let summary = Summary::of(&samples);

    println!(
        "area={:.2} mean={:.2} stddev={:.2}",
        circle.area(),
        summary.mean,
        summary.stddev
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_area_of_the_unit_circle_is_pi() {
        assert!((Circle::new(1.0).area() - std::f64::consts::PI).abs() < 1e-12);
    }

    #[test]
    fn a_constant_sample_has_no_spread() {
        let summary = Summary::of(&[7.0, 7.0, 7.0]);
        assert_eq!(summary.mean, 7.0);
        assert_eq!(summary.stddev, 0.0);
    }
}
"#;

const RUST_GEOMETRY: &str = r#"//! Shapes.

/// A circle, defined by its radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Circle {
    radius: f64,
}

impl Circle {
    /// A circle of the given radius.
    pub fn new(radius: f64) -> Circle {
        Circle { radius: radius.abs() }
    }

    /// The radius.
    pub fn radius(&self) -> f64 {
        self.radius
    }

    /// The enclosed area.
    pub fn area(&self) -> f64 {
        std::f64::consts::PI * self.radius * self.radius
    }

    /// The distance around it.
    pub fn circumference(&self) -> f64 {
        2.0 * std::f64::consts::PI * self.radius
    }

    /// Whether a point lies inside, taking the centre as the origin.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x * x + y * y <= self.radius * self.radius
    }
}
"#;

const RUST_STATS: &str = r#"//! Descriptive statistics.

/// Mean and spread of a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    /// Arithmetic mean.
    pub mean: f64,
    /// Population standard deviation.
    pub stddev: f64,
    /// How many values went into it.
    pub count: usize,
}

impl Summary {
    /// Summarise a sample. An empty sample has a mean of zero rather than NaN,
    /// because a NaN here propagates into every downstream number.
    pub fn of(samples: &[f64]) -> Summary {
        if samples.is_empty() {
            return Summary { mean: 0.0, stddev: 0.0, count: 0 };
        }

        let count = samples.len();
        let mean = samples.iter().sum::<f64>() / count as f64;
        let variance =
            samples.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / count as f64;

        Summary { mean, stddev: variance.sqrt(), count }
    }

    /// The largest value, or `None` for an empty sample.
    pub fn max(samples: &[f64]) -> Option<f64> {
        samples.iter().copied().fold(None, |best, value| match best {
            Some(best) if best >= value => Some(best),
            _ => Some(value),
        })
    }
}
"#;

const PYTHON_ANALYSE: &str = r#"#!/usr/bin/env python3
"""Summarise the fixture's sample records."""

from util import Record, mean


def load_records():
    """The sample data, inline so the script needs no data file."""
    return [
        Record("alpha", 10),
        Record("beta", 20),
        Record("gamma", 30),
        Record("delta", 40),
        Record("epsilon", 50),
    ]


def main():
    records = load_records()
    total = sum(record.value for record in records)
    print(f"records={len(records)} total={total} mean={mean(records):.1f}")


if __name__ == "__main__":
    main()
"#;

const PYTHON_UTIL: &str = r#""""Shared helpers."""

from dataclasses import dataclass


@dataclass
class Record:
    """One named measurement."""

    name: str
    value: int

    def scaled(self, factor):
        """This record with its value multiplied."""
        return Record(self.name, self.value * factor)


def mean(records):
    """The mean value across records, or 0.0 when there are none."""
    if not records:
        return 0.0
    return sum(record.value for record in records) / len(records)


def largest(records):
    """The record with the highest value, or None."""
    return max(records, key=lambda record: record.value, default=None)
"#;

const JS_INDEX: &str = r#"// The fixture's JavaScript entry point.

const { sieve, sum } = require("./lib.js");

function main() {
  const primes = sieve(50);
  console.log(`primes<50=${primes.length} sum=${sum(primes)}`);
}

main();
"#;

const JS_LIB: &str = r#"// Shared helpers.

/** Every prime below `limit`, by the sieve of Eratosthenes. */
function sieve(limit) {
  const composite = new Array(limit).fill(false);
  const primes = [];

  for (let n = 2; n < limit; n += 1) {
    if (composite[n]) continue;
    primes.push(n);
    for (let multiple = n * n; multiple < limit; multiple += n) {
      composite[multiple] = true;
    }
  }

  return primes;
}

/** The sum of an array of numbers. */
function sum(values) {
  return values.reduce((total, value) => total + value, 0);
}

module.exports = { sieve, sum };
"#;

const GO_MAIN: &str = r#"// Command report summarises a block of text.
package main

import (
	"fmt"
	"strings"
)

const document = `Nebula is a native editor.
It renders on the GPU.
It falls back to the CPU.
Its performance budget is comprehensive.`

func main() {
	lines := strings.Split(strings.TrimSpace(document), "\n")
	words := 0
	longest := ""

	for _, line := range lines {
		for _, word := range strings.Fields(line) {
			words++
			trimmed := strings.Trim(word, ".,")
			if len(trimmed) > len(longest) {
				longest = trimmed
			}
		}
	}

	fmt.Printf("lines=%d words=%d longest=%s\n", len(lines), words, longest)
}
"#;

const C_SIEVE: &str = r#"/* Counts the primes below a fixed limit. */
#include <stdio.h>
#include <string.h>

#define LIMIT 1000

int main(void) {
    char composite[LIMIT];
    memset(composite, 0, sizeof composite);

    int count = 0;
    for (int n = 2; n < LIMIT; ++n) {
        if (composite[n]) {
            continue;
        }
        ++count;
        for (long multiple = (long)n * n; multiple < LIMIT; multiple += n) {
            composite[multiple] = 1;
        }
    }

    printf("primes below %d: %d\n", LIMIT, count);
    return 0;
}
"#;

const SHELL_SUMMARY: &str = r#"#!/bin/sh
# Counts the source files in the fixture.
set -eu

count=0
for file in src/main.rs src/geometry.rs src/stats.rs \
            scripts/analyse.py scripts/util.py \
            web/index.js web/lib.js \
            cmd/report/main.go native/sieve.c \
            config/settings.json config/limits.toml README.md; do
    if [ -f "$file" ]; then
        count=$((count + 1))
    fi
done

echo "sources=$count"
"#;

const CONFIG_JSON: &str = r#"{
  "name": "nebula-fixture",
  "version": "0.1.0",
  "limits": {
    "max_open_files": 64,
    "max_file_bytes": 33554432
  },
  "languages": ["rust", "python", "javascript", "go", "c"]
}
"#;

const CONFIG_TOML: &str = r#"# Resource limits for the fixture's tools.

[timeouts]
build = "120s"
run = "30s"

[memory]
max_bytes = 536870912

[sandbox]
network = "denied"
"#;

const README: &str = r#"# nebula-fixture

A small multi-language project the Nebula stress harness generates, edits,
indexes, searches and runs.

Every program here is real: the harness compiles each one, executes it, and
checks what it printed. A fixture that does not build is a harness that is not
testing anything.

## Layout

| Path | Language | What it does |
| --- | --- | --- |
| `src/main.rs` | Rust | Prints an area and a summary |
| `scripts/analyse.py` | Python | Summarises inline records |
| `web/index.js` | JavaScript | Counts primes below 50 |
| `cmd/report/main.go` | Go | Summarises a block of text |
| `native/sieve.c` | C | Counts primes below 1000 |
| `scripts/summary.sh` | Shell | Counts the source files |
| `src/generated.rs` | Rust | Deliberately enormous |
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_fixture_writes_every_file_it_promises() {
        let dir = TempDir::new().unwrap();
        let fixture = Fixture::generate(dir.path(), 100).unwrap();

        for program in &fixture.programs {
            assert!(
                dir.path().join(&program.entry).is_file(),
                "{} was not written",
                program.entry.display()
            );
        }
        for source in fixture.sources() {
            assert!(source.is_file(), "{} was not written", source.display());
        }
    }

    #[test]
    fn the_generated_file_is_the_size_it_claims() {
        let dir = TempDir::new().unwrap();
        Fixture::generate(dir.path(), 4_000).unwrap();

        let text = std::fs::read_to_string(dir.path().join("src/generated.rs")).unwrap();
        let lines = text.lines().count();
        // Six lines per function plus a preamble; the point is the order of
        // magnitude, not the exact count.
        assert!(lines > 4_000, "only {lines} lines");
        assert!(lines < 12_000, "{lines} lines is more than asked for");
    }

    #[test]
    fn the_generated_file_parses_as_rust() {
        // A large file that the parser rejects would make every timing in the
        // report meaningless.
        let dir = TempDir::new().unwrap();
        Fixture::generate(dir.path(), 2_000).unwrap();

        let text = std::fs::read_to_string(dir.path().join("src/generated.rs")).unwrap();
        let buffer = nebula_core::TextBuffer::from_str(&text);
        let grammar = nebula_syntax::GrammarRegistry::new().get("rust").unwrap();
        let tree = nebula_syntax::SyntaxTree::parse(grammar, &buffer, 0).unwrap();

        assert!(!tree.has_error(), "the generated file does not parse");
    }

    #[test]
    fn every_hand_written_source_parses_in_its_own_language() {
        let dir = TempDir::new().unwrap();
        Fixture::generate(dir.path(), 50).unwrap();
        let registry = nebula_syntax::GrammarRegistry::new();

        for relative in [
            "src/main.rs",
            "src/geometry.rs",
            "src/stats.rs",
            "scripts/analyse.py",
            "scripts/util.py",
            "web/index.js",
            "web/lib.js",
            "cmd/report/main.go",
            "native/sieve.c",
            "config/settings.json",
        ] {
            let path = dir.path().join(relative);
            let language = nebula_core::document::detect_language(&path)
                .unwrap_or_else(|| panic!("no language detected for {relative}"));
            let Ok(grammar) = registry.get(&language) else { continue };

            let text = std::fs::read_to_string(&path).unwrap();
            let buffer = nebula_core::TextBuffer::from_str(&text);
            let tree = nebula_syntax::SyntaxTree::parse(grammar, &buffer, 0).unwrap();

            assert!(!tree.has_error(), "{relative} does not parse as {language}");
        }
    }

    #[test]
    fn the_shell_script_is_executable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            Fixture::generate(dir.path(), 10).unwrap();
            let mode = std::fs::metadata(dir.path().join("scripts/summary.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn the_fixture_is_byte_identical_between_runs() {
        // Two runs of the harness have to be comparable, which means the input
        // cannot drift.
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        Fixture::generate(a.path(), 500).unwrap();
        Fixture::generate(b.path(), 500).unwrap();

        for relative in ["src/main.rs", "src/generated.rs", "web/lib.js"] {
            assert_eq!(
                std::fs::read(a.path().join(relative)).unwrap(),
                std::fs::read(b.path().join(relative)).unwrap(),
                "{relative} differs between runs"
            );
        }
    }

    #[test]
    fn every_program_names_the_tool_it_needs() {
        // The harness skips a program whose toolchain is absent rather than
        // failing, so the name has to be there to check.
        for program in programs() {
            assert!(!program.requires.is_empty(), "{} names no tool", program.language);
            assert!(!program.expect.is_empty(), "{} expects no output", program.language);
            assert!(!program.run.is_empty());
        }
    }
}
