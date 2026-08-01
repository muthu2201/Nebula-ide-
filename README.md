# Nebula

A native, GPU-accelerated, AI-native code editor written in Rust.

Twenty-two crates, **1 083 tests**, and an end-to-end stress harness that drives
a real editor over a real project and compiles and runs the programs in it. No
mocked services anywhere: HTTP is tested over real sockets, the sandbox against
the real kernel, the WASM host against real components, and the renderer against
real pixels.

---

## What it does

* **Native rendering.** wgpu on the GPU, tiny-skia on the CPU when there is no
  GPU to be had. Not a degraded fallback — the same scene, rasterised
  differently. Everything renders either way.
* **Bring your own key.** Requests go directly from your machine to the model
  provider. No Nebula proxy is on the path. Keys live in the OS keychain.
* **Three rings of sandboxing.** A WASM capability sandbox, OS-level confinement
  (Landlock + seccomp, Seatbelt, Job Objects), and resource limits. Where a ring
  cannot enforce, it says so rather than pretending.
* **Extensions as signed WebAssembly components.** Wasmtime 47, the Component
  Model, a published WIT world, Ed25519 publisher signatures, and notarisation
  that checks declared capabilities against the imports a component actually has.
* **Nine bundled tree-sitter grammars**, incremental parsing, and
  highlighting that stays off the keystroke path.
* **Six release targets**: Linux, macOS and Windows, x86-64 and ARM64.

## What it costs

Measured on the software renderer at 1920×1080, on a four-core container,
against a 300 007-line file. See [the full report](stress-results/REPORT.md).

| | Budget | Measured |
| --- | ---: | ---: |
| Keystroke to photon (p50) | 16 ms | **7.3 ms** |
| Keystroke to photon (p95) | 16 ms | **11.0 ms** |
| Same, on 300 007 lines (p95) | 16 ms | **6.2 ms** |
| Cold start to first frame | 500 ms | **14.1 ms** |

All six fixture programs — Rust, C, Go, Python, JavaScript and shell — build and
run correctly under the sandbox.

---

## Getting started

```sh
git clone <this repository>
cd Nebula-ide-

cargo build --release
cargo run --release --bin nebula -- .
```

On Linux you need the windowing headers first:

```sh
sudo apt-get install -y libx11-dev libxkbcommon-dev libwayland-dev pkg-config
```

### What this machine supports

```sh
cargo run --release --bin nebula -- doctor
```

Reports the renderer and *why* it was chosen, the sandbox backend and whether it
can enforce anything, the bundled grammars, which providers have a key stored,
and where configuration lives. Run this first when something behaves differently
on one machine.

### Without a display

Every subcommand except the default is headless, and the whole editor builds
without a windowing library at all:

```sh
cargo build --release --no-default-features -p nebula-ide

nebula render src/main.rs -o screenshot.png     # Rasterise a file
nebula run session.nbs --budget-ms 16           # Replay a scripted session
nebula doctor                                   # Probe the machine
```

### Run the stress test

```sh
cargo build --release -p nebula-stress
./target/release/nebula-stress
```

About a minute on four cores. Writes `stress-results/report.json` and
`stress-results/REPORT.md`, and exits non-zero if anything misses its budget.

---

## Keys

| Key | Action |
| --- | --- |
| <kbd>Ctrl/Cmd</kbd>+<kbd>S</kbd> | Save |
| <kbd>Ctrl/Cmd</kbd>+<kbd>Z</kbd> / <kbd>⇧</kbd>+<kbd>Z</kbd> | Undo / redo |
| <kbd>Ctrl/Cmd</kbd>+<kbd>D</kbd> / <kbd>L</kbd> / <kbd>A</kbd> | Select word / line / all |
| <kbd>Ctrl/Cmd</kbd>+<kbd>Alt</kbd>+<kbd>↑</kbd>/<kbd>↓</kbd> | Add a cursor above / below |
| <kbd>Ctrl/Cmd</kbd>+<kbd>P</kbd> / <kbd>⇧</kbd>+<kbd>P</kbd> | Find file / command palette |
| <kbd>Ctrl/Cmd</kbd>+<kbd>F</kbd> / <kbd>K</kbd> | Search / AI panel |
| <kbd>Ctrl</kbd>+<kbd>←</kbd>/<kbd>→</kbd> (<kbd>Alt</kbd> on macOS) | Word-wise motion |
| <kbd>Home</kbd> | First non-whitespace, then column 0 |

<kbd>Shift</kbd> turns any motion into a selection.

---

## Building an extension

```sh
cargo run --release --bin nebula-sdk -- new my-extension --id com.example.thing
cd my-extension
rustup target add wasm32-wasip2

nebula-sdk build      # Compile to a component
nebula-sdk test       # Load it into the editor's real host
nebula-sdk check      # Run the registry's notarisation locally
nebula-sdk package --key ~/.nebula/publisher.key
```

`nebula-sdk test` loads the extension into the same Wasmtime host the editor
uses, with the same capability checks and the same execution limits, so
behaviour there matches behaviour in the editor.

---

## Documentation

| Document | What it covers |
| --- | --- |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it is put together, and why each contested decision went the way it did |
| [docs/INFRASTRUCTURE.md](docs/INFRASTRUCTURE.md) | CI/CD, release targets, signing, reproducing every number |
| [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) | What the sandbox defends against, and what it does not |
| [docs/STRESS_TESTING.md](docs/STRESS_TESTING.md) | The harness, the fixture, and what a run found |
| [stress-results/REPORT.md](stress-results/REPORT.md) | The most recent recorded run |

Both the architecture document and the threat model carry an explicit
"what this does not do" section. A design document that only lists strengths is
marketing.

---

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p nebula-ide --no-default-features   # the headless build
```

The workspace uses Rust 2024 and requires the toolchain named in
`rust-version` in the root `Cargo.toml`.

---

## Licence

Proprietary. See `LICENSE`.

The bundled DejaVu Sans Mono font is distributed under the Bitstream Vera
licence; see `crates/nebula-render/assets/fonts/LICENSE-DejaVu.txt`.
