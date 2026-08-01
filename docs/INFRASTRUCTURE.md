# Infrastructure

How Nebula is built, checked, packaged and shipped — and how to reproduce any of
it on a local machine.

---

## Repository layout

```
Cargo.toml              Workspace: `members = ["crates/*"]`, edition 2024
deny.toml               Dependency policy enforced by cargo-deny
rustfmt.toml            Formatting, enforced in CI
.github/
  workflows/
    ci.yml              Format, clippy, tests on four runners, docs, MSRV
    security.yml        cargo-audit, cargo-deny, unused dependencies
    stress.yml          The end-to-end run, on four runner/renderer pairs
    release.yml         Six-target release matrix, signing, SBOM, attestation
  dependabot.yml        Grouped dependency updates
crates/                 Twenty-two crates
docs/                   This documentation
stress-results/         The committed report from the most recent recorded run
```

The workspace excludes `extensions/`: extension crates target
`wasm32-wasip2` and are built by `nebula-sdk` against the published WIT world,
never as part of the host build.

---

## Build profiles

| Profile | Used for | Settings |
| --- | --- | --- |
| `dev` | Everyday work | Default, plus optimised dependencies |
| `release` | Shipping | `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, stripped |
| `release-unwind` | Anything needing a backtrace in a release build | As `release`, `panic = "unwind"` |
| `bench` | Criterion | Inherits `release`, debug symbols kept |
| `stress` | The harness | Inherits `release`, debug symbols kept |

`panic = "abort"` in `release` is deliberate: an editor that continues after a
panic is an editor that may write a corrupted file. `release-unwind` exists for
the cases where a backtrace matters more.

---

## Continuous integration

### `ci.yml` — must pass before anything lands

| Job | Runner | What it does |
| --- | --- | --- |
| `format` | ubuntu-24.04 | `cargo fmt --all --check` |
| `clippy` | ubuntu-24.04 | `cargo clippy --workspace --all-targets --all-features` with `-D warnings` |
| `test` | ubuntu-24.04, ubuntu-24.04-arm, macos-15, windows-2025 | `cargo test --workspace --all-features` |
| `docs` | ubuntu-24.04 | `cargo doc` with `-D warnings -D rustdoc::broken_intra_doc_links` |
| `msrv` | ubuntu-24.04 | `cargo check` on the toolchain named in `rust-version` |

The jobs are separate so a failure names its own cause. A formatting job that
also runs the tests tells you "CI failed" and nothing else.

Two environment gates matter on Linux:

* **`NEBULA_REQUIRE_SANDBOX=1`** turns the sandbox tests from "skip if Landlock
  is missing" into "fail if Landlock is missing". Without it, a kernel
  regression would silently turn the whole sandbox suite into no-ops.
* **`NEBULA_REQUIRE_GPU=1`**, together with lavapipe as the Vulkan ICD, does the
  same for the wgpu path. On a developer's laptop those tests skip when no
  adapter is present; in CI an adapter always exists, so skipping is a failure.

The `test` job also runs `cargo check -p nebula-ide --no-default-features`, which
builds the editor without winit at all. That is the configuration a container or
a server uses, and it breaks the moment someone reaches for a window type from
the wrong layer.

### `security.yml` — supply chain

Runs on pushes, on pull requests that touch a manifest, and daily at 06:17 UTC.

| Job | Tool | Policy |
| --- | --- | --- |
| `audit` | `cargo-audit` | `--deny warnings`; a yanked crate fails |
| `deny` | `cargo-deny` | Bans, licences, sources, advisories |
| `unused` | `cargo-udeps` | Reports, does not block — it has false positives on `cfg`-gated crates |

It is a separate workflow from CI because these fail for reasons outside a pull
request's control: an advisory published overnight against a dependency nobody
touched should wake up a scheduled run, not block whoever pushes next.

**Licence policy** (`deny.toml`): permissive only — Apache-2.0, BSD, ISC, MIT,
MPL-2.0, Unicode-3.0, Zlib and friends. Copyleft is denied. A copyleft dependency
in a proprietary editor is a licensing problem discovered at the worst possible
moment.

**Banned crates**: `openssl` and `openssl-sys`. Everything that speaks TLS uses
rustls, so a transitive dependency dragging in a system OpenSSL is a build that
fails on a machine without it.

### `stress.yml` — does it actually work

The job that decides whether the editor works, rather than whether it compiles.
Runs on pushes to `main`, on pull requests touching `crates/`, and nightly at
03:41 UTC.

| Runner | Renderer | Why |
| --- | --- | --- |
| ubuntu-24.04 | GPU (lavapipe) | Exercises the wgpu path |
| ubuntu-24.04 | CPU | The fallback is the path most likely to rot — nobody uses it on their own machine |
| macos-15 | GPU (Metal) | The only real GPU driver in the matrix |
| windows-2025 | CPU | Windows path resolution and process spawning differ enough to matter |

Every runner installs Go 1.25, Node 22 and Python 3.13 alongside the Rust
toolchain and `cc`, and the harness runs with `--require-all-toolchains`, which
turns a missing compiler into a failure rather than a skip. A runner image that
quietly drops a toolchain would otherwise shrink the test without anyone
noticing.

Each run publishes its Markdown report to the job summary and keeps both report
formats as artefacts for 30 days.

A `benchmarks` job records Criterion numbers in bencher format. They are recorded
rather than gated on: Criterion's output is only comparable against a baseline
from the same runner class, and a hard threshold on a shared runner produces
noise, not signal.

---

## Releases

`release.yml`, triggered by a `v*` tag or manually with an explicit version.

### The six targets

| Target | Runner | Archive |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | ubuntu-24.04 | `.tar.gz` |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | `.tar.gz` |
| `x86_64-apple-darwin` | macos-15 | `.tar.gz` |
| `aarch64-apple-darwin` | macos-15 | `.tar.gz` |
| `x86_64-pc-windows-msvc` | windows-2025 | `.zip` |
| `aarch64-pc-windows-msvc` | windows-11-arm | `.zip` |

Native runners for every target rather than cross-compilation. Cross-building is
faster right up until a linker difference produces an artefact that only fails on
a user's machine.

Each archive carries the `nebula` and `nebula-sdk` binaries, the README, the
licence and the `docs/` tree.

### Signing

| Platform | Mechanism | Secrets |
| --- | --- | --- |
| macOS | `codesign --options runtime --timestamp`, then notarisation | `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGNING_IDENTITY`, `MACOS_NOTARY_PROFILE` |
| Windows | `signtool` with an RFC 3161 timestamp | `WINDOWS_CERTIFICATE`, `WINDOWS_CERTIFICATE_PASSWORD` |
| Update manifest | Ed25519, verified by `nebula-update` | `NEBULA_UPDATE_KEY` |

Signing runs **only where the secret exists**, and emits a workflow warning where
it does not. A fork can build a release; it simply gets an unsigned one, and the
log says so. An unsigned build that claims to be signed is worse than one that
admits it.

The signing keychain is created fresh in `RUNNER_TEMP`, unlocked with a random
password, and deleted in the same step. The certificate is written to disk only
for the duration of the import.

### What ships alongside the binary

* **`SHA256SUMS`** — one file covering every artefact in the release.
* **An SBOM per target**, CycloneDX JSON, generated by `cargo-cyclonedx`.
* **A build-provenance attestation** via `actions/attest-build-provenance`,
  which records what built the artefact and from which commit.

Releases are published as **drafts**. A release that publishes itself is a
release nobody read before it went out.

### Smoke test before publishing

Every artefact that can run on its build runner is executed:

```
nebula --version
nebula doctor
```

`doctor` exercises the renderer selection, the sandbox probe, the grammar
registry and the keychain lookup. A binary that cannot get through it is broken
in a way that matters, and catching that before publication is worth the twenty
seconds.

---

## Reproducing any of it locally

```sh
# What CI checks, in the order CI checks it
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --no-deps

# The headless build — no winit, no display server
cargo check -p nebula-ide --no-default-features

# Dependency policy
cargo install cargo-deny cargo-audit --locked
cargo deny check
cargo audit

# The end-to-end stress run (this is the one that matters)
cargo build --release -p nebula-stress
./target/release/nebula-stress

# …with the sandbox and GPU tests made mandatory, as in CI
NEBULA_REQUIRE_SANDBOX=1 NEBULA_REQUIRE_GPU=1 cargo test --workspace
```

The stress run writes `stress-results/report.json` and
`stress-results/REPORT.md`. It takes about a minute on four cores.

---

## Running the editor

```sh
cargo run --release --bin nebula -- path/to/project     # GUI
cargo run --release --bin nebula -- doctor              # What this machine supports
cargo run --release --bin nebula -- render f.rs -o s.png  # Headless screenshot
cargo run --release --bin nebula -- run session.nbs     # Replay a scripted session
```

`nebula doctor` reports the renderer and why it was chosen, the sandbox backend
and whether it can enforce anything, the bundled grammars, which providers have a
key in the keychain, and where configuration lives. It is the first thing to run
when something behaves differently on one machine.

---

## Configuration and state

| Path | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/nebula/config.json` (Linux) | Settings |
| `~/Library/Application Support/nebula/` (macOS) | Settings |
| `%APPDATA%\nebula\` (Windows) | Settings |
| `$NEBULA_CONFIG_DIR` | Overrides all of the above |
| OS keychain | API keys — never a file |
| `<project>/.nebula/` | Per-project caches and indexes |

`$NEBULA_CONFIG_DIR` exists so the test suite and the stress harness get an
isolated configuration without touching the running user's.

A missing config file is the normal first-run state. A malformed one is reported
and ignored — an editor that refuses to open because of a stray comma in a
settings file is an editor you cannot use to fix the settings file. Unknown keys
are reported rather than silently dropped, because silently ignoring a typo'd key
is how someone spends an hour wondering why their setting does nothing.

| Variable | Effect |
| --- | --- |
| `NEBULA_LOG` | `tracing` filter, e.g. `nebula_render=debug` |
| `NEBULA_CONFIG_DIR` | Configuration directory |
| `NEBULA_REQUIRE_SANDBOX` | Sandbox tests fail rather than skip |
| `NEBULA_REQUIRE_GPU` | GPU tests fail rather than skip |
| `NEBULA_REGISTRY` | Extension registry URL |
| `NEBULA_PUBLISHER_KEY` | Path to the extension signing key |

---

## Dependency management

Dependabot runs weekly for Cargo and monthly for Actions, grouped so that related
crates arrive in one reviewable pull request:

* **graphics** — `wgpu*`, `winit`, `cosmic-text`, `tiny-skia`, `swash`, `etagere`
* **tree-sitter** — every grammar and the runtime
* **async** — `tokio*`, `futures*`, `hyper*`, `axum`, `tower*`, `reqwest`
* **minor-and-patch** — everything else, batched

The graphics group in particular only makes sense reviewed together: wgpu, winit
and cosmic-text move as a set, and a partial bump is a compile error waiting for
whoever merges second.

---

## Bootstrapping a fresh machine

```sh
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup toolchain install stable
rustup target add wasm32-wasip2          # for building extensions

# Linux: the windowing and graphics headers
sudo apt-get install -y libx11-dev libxkbcommon-dev libwayland-dev pkg-config
sudo apt-get install -y mesa-vulkan-drivers   # a software Vulkan adapter

# macOS
xcode-select --install

# Windows
# Visual Studio Build Tools with the "Desktop development with C++" workload
```

The stress harness additionally needs `cc`, `go`, `node`, `python3` and `sh` to
run all six of its programs. Any that are missing are skipped with a note, unless
`--require-all-toolchains` is passed.
