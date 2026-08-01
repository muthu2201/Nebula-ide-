# Stress testing

`nebula-stress` drives a real editor over a real project and runs the programs in
it. Nothing in it stands in for anything: the keystrokes go through the real
keymap, the frames are real rasterised pixels, and the six programs are compiled
and executed as real child processes with their output checked.

A harness that substitutes a fake for the expensive part measures the fake.

---

## Running it

```sh
cargo build --release -p nebula-stress
./target/release/nebula-stress
```

| Flag | Default | What it does |
| --- | --- | --- |
| `--workdir` | `stress-results/fixture` | Where the fixture project is written |
| `--generated-lines` | `200000` | Size of the generated file |
| `--keystrokes` | `5000` | Keystrokes the editing phase delivers |
| `--surface` | `1920x1080` | Surface size in pixels |
| `--cpu` | off | Force the software renderer |
| `--skip-programs` | off | Skip compiling and running the fixture's programs |
| `--require-all-toolchains` | off | Fail rather than skip when a compiler is missing |
| `--json` | `stress-results/report.json` | Machine-readable report |
| `--markdown` | `stress-results/REPORT.md` | Human-readable report |

The process exits non-zero if any measurement misses its budget or any program
prints something other than what the fixture expects.

---

## The fixture

A multi-language project written fresh on every run, byte-identical between runs
so two reports are comparable.

| Path | Language | What it does | Expected output |
| --- | --- | --- | --- |
| `src/main.rs` | Rust | Circle area and a sample summary | `area=78.54 mean=3.00 stddev=1.41` |
| `scripts/analyse.py` | Python | Summarises inline records | `records=5 total=150 mean=30.0` |
| `web/index.js` | JavaScript | Primes below 50 | `primes<50=15 sum=328` |
| `cmd/report/main.go` | Go | Word statistics over a block of text | `lines=4 words=21 longest=comprehensive` |
| `native/sieve.c` | C | Primes below 1000 | `primes below 1000: 168` |
| `scripts/summary.sh` | Shell | Counts the source files | `sources=12` |

Plus `src/geometry.rs`, `src/stats.rs`, `scripts/util.py`, `web/lib.js`, a
`Cargo.toml`, a `.gitignore`, JSON and TOML config, a README — and
`src/generated.rs`, which is 200 000 lines of distinct, compiling Rust.

The generated file is distinct rather than repeated on purpose: a large file made
of identical lines lets tree-sitter shortcut the parse and measures nothing.

Rust is built with `rustc` directly rather than `cargo`, so the harness never
needs network access to fetch a registry index.

---

## The six phases

### 1. Cold start

Construct an editor and draw the first frame. Budget: 500 ms.

### 2. Editing

Open `src/stats.rs`, then deliver every keystroke through `App::key` and draw a
full frame after each one. The measurement is **keystroke to finished pixels** —
anything less than the whole path is a number that looks good and means nothing.

Also exercises a 64-cursor edit and a full undo of everything typed, because
multi-cursor and undo are the operations most likely to be accidentally
quadratic.

Budget: 16 ms on the CPU renderer, 8 ms on the GPU, at p50 and p95.

### 3. Large file

The same work on `src/generated.rs`: open it, scroll the whole way through, then
type into the middle. This is the phase that tests the claim that per-frame cost
tracks the window rather than the document.

### 4. Indexing

Walk the project, build the Personalized-PageRank repo map, run a content search,
embed every file and build and query an HNSW index. Fails if the repo map ranks
nothing, if searching a tree full of Rust finds no `pub fn`, or if the vector
index returns nothing.

### 5. Programs

Compile and run each of the six programs under `Policy::project_tool` — the same
sandbox profile an agent tool call gets — and compare the output against what the
fixture says it should be. A missing toolchain is skipped with a note, unless
`--require-all-toolchains` is set.

### 6. Durability

Save every open file, read it back and compare byte for byte, then re-parse each
of the fixture's sources to confirm that everything the harness typed left them
still valid in their own language.

---

## What a run found

The first full run failed, which is the only reason to write a harness like this.
Four real defects, all now fixed and all now covered by regression tests:

| Defect | Symptom | Fix |
| --- | --- | --- |
| Blanket seccomp network denial | Every sandboxed build tool failed to spawn its linker — Rust's std builds its CLOEXEC pipe with `socketpair(AF_UNIX)` | Filter by address family: allow `AF_UNIX`, refuse the rest |
| Relative program resolution | `./nebula-fixture-sieve` reported missing though it existed | Resolve against the command's working directory, not the process's |
| Keystroke cost on a large file | 487 ms per keystroke on 300 007 lines | Documents record the transactions they applied; re-parsing moved off the keystroke path |
| Per-pixel rasteriser calls | 17.9 ms per frame at 1920×1080 | Composite glyph coverage directly instead of a `fill_rect` per covered pixel |

The last two together: **487 ms → 5.8 ms** on the large file, and
**17.9 ms → 7.3 ms** on a small one.

---

## In CI

`stress.yml` runs the harness on four runner/renderer pairs — Linux with lavapipe
and with the CPU fallback, macOS with Metal, Windows with the CPU fallback — on
every push to `main`, on pull requests touching `crates/`, and nightly.

Every runner installs Go, Node and Python alongside Rust and `cc`, and the
harness runs with `--require-all-toolchains`, so a runner image that quietly
drops a compiler fails the job rather than shrinking the test.

Reports go to the job summary and are kept as artefacts for 30 days.

---

## Session scripts

For a targeted scenario rather than the whole harness, `nebula run` replays a
plain-text script through the same code paths a keyboard drives:

```
open src/main.rs
end
type \n// a trailing comment
key primary+s
frame
expect-contains // a trailing comment
expect-saved
```

```sh
nebula run session.nbs --root . --report timings.json --budget-ms 16
```

| Command | What it does |
| --- | --- |
| `open PATH` | Open a file, relative to `--root` |
| `type TEXT` | Type it, one character at a time; `\n` is Enter |
| `key SPEC` | Press a key: `ctrl+s`, `shift+left`, `primary+p`, `f5` |
| `repeat N` | Do the previous step N more times |
| `frame` | Draw a frame and record how long it took |
| `save` | Save the focused document |
| `home` / `end` / `start` | Move the caret |
| `select-all` / `undo` / `redo` / `cursor-below` | Editing actions |
| `scroll N` | Scroll without moving the caret |
| `expect-contains TEXT` | Fail unless the document contains it |
| `expect-length N` | Fail unless the document is N characters |
| `expect-saved` | Fail unless there are no unsaved changes |

`primary` resolves to Cmd on macOS and Ctrl everywhere else, so one script runs
on every platform.
