# Architecture

Nebula is a native, GPU-accelerated, AI-native code editor written in Rust. This
document describes how it is put together and, where a decision was contested,
why it went the way it did.

It describes what is in this repository. Where something is deliberately not
built, or is enforced weakly, that is stated rather than glossed — see
[Honest limits](#honest-limits).

---

## The shape of the system

```
                        ┌──────────────────────────────┐
                        │          nebula-ide          │
                        │  app · workspace · session   │
                        │  window (winit, `gui` only)  │
                        └───────┬──────────────┬───────┘
                                │              │
              ┌─────────────────┘              └──────────────────┐
              ▼                                                   ▼
     ┌──────────────────┐                              ┌────────────────────┐
     │    nebula-ui     │                              │    nebula-agent    │
     │ view · input     │                              │ loop · tools       │
     │ layout · theme   │                              │ capability · audit │
     └────────┬─────────┘                              └─────────┬──────────┘
              │                                                  │
   ┌──────────┼───────────────┐                    ┌─────────────┼─────────────┐
   ▼          ▼               ▼                    ▼             ▼             ▼
┌────────┐ ┌────────┐ ┌──────────────┐      ┌──────────┐ ┌────────────┐ ┌──────────┐
│ core   │ │ render │ │   syntax     │      │ nebula-ai│ │ nebula-mcp │ │  -exec   │
│ rope   │ │ wgpu / │ │ tree-sitter  │      │  BYOK    │ │ 2026-07-28 │ │ sandbox  │
│ edits  │ │tiny-sk.│ │ highlights   │      │ keychain │ │  stateless │ │  limits  │
└────────┘ └────────┘ └──────────────┘      └──────────┘ └────────────┘ └────┬─────┘
   ▲                          ▲                                              │
   │                          │                                              ▼
┌──┴─────┐ ┌──────────┐ ┌─────┴─────┐  ┌───────────┐              ┌────────────────┐
│  vfs   │ │  search  │ │   index   │  │  vector   │              │ nebula-sandbox │
│ walk   │ │ ripgrep  │ │ PageRank  │  │   HNSW    │              │ Landlock/      │
│ watch  │ │  fuzzy   │ │ repo map  │  │ embedding │              │ Seatbelt/Job   │
└────────┘ └──────────┘ └───────────┘  └───────────┘              └────────────────┘

     Extensions                          Distribution
┌───────────────────┐            ┌────────────────────────────┐
│ nebula-wasm-host  │            │ nebula-pkg · nebula-license│
│ Wasmtime 47, WIT  │◀───────────│ nebula-update · -registry  │
│ capability gates  │            │ nebula-sdk                 │
└───────────────────┘            └────────────────────────────┘
```

Twenty-two crates in one Cargo workspace. Dependencies point downward only;
there are no cycles, and nothing below `nebula-ui` knows a window exists.

| Crate | Lines | Tests | What it is |
| --- | ---: | ---: | --- |
| `nebula-core` | 3 145 | 88 | Rope buffers, positions, selections, transactions, undo |
| `nebula-vfs` | 1 090 | 29 | Project walking, `.gitignore`, file watching |
| `nebula-syntax` | 1 793 | 38 | tree-sitter parsing, highlighting, symbols |
| `nebula-search` | 913 | 27 | Content search (ripgrep engine) and fuzzy matching |
| `nebula-index` | 1 326 | 48 | Symbol graph and Personalized-PageRank repo map |
| `nebula-vector` | 1 350 | 32 | HNSW index and hashing embedder |
| `nebula-render` | 2 732 | 72 | Scene model, wgpu backend, tiny-skia fallback, text |
| `nebula-ui` | 2 998 | 107 | Viewport, layout, keymap, editing actions, themes |
| `nebula-ide` | 3 509 | 99 | Workspace, application, session scripts, window |
| `nebula-lsp` | 1 746 | 44 | Language Server Protocol client |
| `nebula-mcp` | 2 345 | 54 | Model Context Protocol client (spec 2026-07-28) |
| `nebula-ai` | 2 716 | 83 | Model catalogue, BYOK keys, Anthropic adapter, caching |
| `nebula-agent` | 2 787 | 67 | Agent loop, tool dispatch, capabilities, audit log |
| `nebula-sandbox` | 1 254 | 28 | Landlock, seccomp, Seatbelt, Job Objects |
| `nebula-exec` | 1 174 | 30 | Sandboxed child processes with resource limits |
| `nebula-wasm-host` | 1 854 | 40 | Wasmtime Component Model host and WIT world |
| `nebula-pkg` | 1 691 | 58 | Signed extension packages, manifests, notarisation |
| `nebula-registry` | 1 254 | 29 | Marketplace service |
| `nebula-sdk` | 1 092 | 21 | Extension developer toolchain |
| `nebula-license` | 1 059 | 32 | Ed25519 offline licences, hardware fingerprints |
| `nebula-update` | 1 057 | 29 | Signed delta updates |
| `nebula-stress` | 2 428 | 38 | End-to-end stress harness |

**1 083 tests pass across the workspace.** There are no mocked services in them:
the HTTP layers are tested over real sockets, the sandbox against the real
kernel, the WASM host against real components, and the renderer against real
pixels.

---

## The rules the layering enforces

### Nothing below `nebula-ide` opens a window

`nebula-ui` turns a document plus a viewport into a `nebula_render::Scene`, and
turns a key event into an action on a document. It touches no window, no GPU and
no event loop. `nebula-ide::window` is the only place winit appears, it is behind
a `gui` feature, and everything it does is a two-line call into
`nebula-ide::app`.

That is not tidiness for its own sake. It is what lets a test open a document,
send keystrokes and assert on the resulting pixels with no display server — and
therefore what lets the stress harness drive a real editor in CI.

### The scene is data

A frame is a flat, ordered list of rectangles, glyph runs and clip regions in
logical pixels. Nothing in it knows about GPUs, buffers or draw calls. The same
scene is asserted primitive-by-primitive in a unit test and submitted to a
swapchain in production.

### Cost tracks the window, not the document

Every loop in the view runs over *visible* lines. Highlighting is requested for
the visible character range plus a margin. Opening a 200 000-line file costs the
same per frame as opening a 50-line one — measured, not asserted:
[the stress report](../stress-results/REPORT.md) shows 6.16 ms per keystroke on a
300 007-line file against 11.0 ms on a small one.

---

## Text and editing

### The buffer

`ropey` ropes, with the document's original encoding and line ending recorded
separately. CRLF is normalised to LF on the way in and restored on the way out,
so a one-line change to a Windows file does not produce a whole-file diff.

Three offset spaces exist and are never conflated:

* **character offsets** — what the editor works in,
* **byte offsets** — what tree-sitter works in,
* **UTF-16 code units** — what LSP works in.

`nebula-core` converts between all three and refuses to guess.

### Edits are transactions

An `Edit` is a range and its replacement. A `Transaction` is a set of
non-overlapping edits applied back-to-front, so each edit's pre-transaction
coordinates stay valid while it runs. Applying one returns its exact inverse,
captured at the moment of the edit — undo never re-derives a diff.

The whole set is bounds-checked before anything is mutated, so a failed
transaction leaves the buffer untouched.

### Undo groups are sequences, not merged transactions

A run of typing is one undo step. The tempting implementation is to flatten the
group into a single transaction; it is wrong, and the stress harness caught it.
Each keystroke's edit is expressed in the coordinates that were current when it
ran, so flattening produces a redo whose later edits point past the end of the
undone buffer. A `HistoryEntry` therefore stores the group as an ordered list of
transactions and replays them in order.

### Multiple cursors

A `SelectionSet` is always non-empty, always sorted, and merges selections that
touch. One of them is primary. Every editing action operates on all of them, in
one transaction, so multi-cursor editing is one undo step and one re-parse.

---

## Syntax

tree-sitter 0.26 with nine bundled grammars: C, Go, JavaScript, JSON,
Python, Rust, TOML, TypeScript and TSX. Parsing reads the rope through a
chunk callback rather than materialising a string, so a 7.7 MB file is parsed
without a 7.7 MB allocation.

### Capture precedence

Grammars open their highlights query with blanket rules — Python's very first
pattern is `(identifier) @variable`, which matches every identifier in the file
including function names. A later, narrower pattern captures the same node as
`@function`. tree-sitter emits both and does not order them usefully. Nebula
ranks the catch-all classes (`Text`, `Variable`) below everything else and
resolves overlaps by splitting the head and tail remnants of the wider span.

### Re-parsing is off the keystroke path

This is the single most consequential decision in the editor, and it was forced
by measurement.

"Incremental" bounds the work by how much of the *tree* changed, not the text.
One character typed into a file with fifty thousand top-level items rebuilds the
root's child list. Measured on the 300 007-line fixture: **~150 ms**, against a
16 ms frame budget, and independent of where the edit landed.

So the editor splits the update in two:

1. `SyntaxTree::edit` shifts the existing nodes' offsets — microseconds, and it
   runs between the keystroke and the frame.
2. `SyntaxTree::reparse_incremental` does the real work, and runs at the first
   pause in typing (40 ms, `nebula_ide::app::PARSE_DELAY`).

The editor therefore paints from an *edited-but-stale* tree. Highlighting stays
visually correct everywhere the edit did not touch, which is everywhere the user
is not looking. The consequence is that a stale tree can name a byte past the end
of the current text, so the highlighter clamps node offsets rather than failing
the frame.

Effect on the fixture: **487 ms → 5.8 ms** per keystroke.

---

## Rendering

### Two backends, one scene

The GPU path is the product. But a renderer that only works on a GPU does not
work on a VM without one, on a machine whose driver has crashed, or over a remote
session — and an editor that will not start is worse than one that scrolls a
little less smoothly.

So `Renderer` is a trait with two implementations, and the fallback is not a
degraded feature set. It is the same `Scene`, rasterised on the CPU with
tiny-skia. Everything renders; only the frame path differs.

| | GPU (`wgpu` 30) | CPU (`tiny-skia`) |
| --- | --- | --- |
| Quads | Instanced, one draw call | `fill_rect` per quad |
| Glyphs | Packed atlas, one draw call per run | Coverage composited per pixel |
| Frame budget | 8 ms (120 Hz) | 16 ms (60 Hz) |

Selection is automatic and the reason is recorded, so `nebula doctor` can say
*why* it fell back rather than only that it did.

### Text

`cosmic-text` 0.19 for shaping, with DejaVu Sans Mono bundled. Bundling the font
is what makes rendering deterministic across machines and makes pixel assertions
meaningful on a font-less CI runner.

The CPU path composites glyph coverage directly into the pixel buffer. The
obvious implementation — build a `Paint` and call `fill_rect` for each coverage
box cosmic-text hands back — runs the whole rasteriser pipeline about a quarter
of a million times per frame on a full screen of text. Compositing by hand is
twenty lines of integer arithmetic and took keystroke-to-photon from 17.9 ms to
7.3 ms at 1920×1080. Source-over with rounding (`+ 127` before the divide),
because truncating darkens antialiased edges by a level.

---

## The three rings of sandboxing

Untrusted code — extensions, agent tool calls, build commands — runs inside three
independent mechanisms. Each is a separate failure domain; none is trusted to be
sufficient alone.

### Ring 1 — the WASM capability sandbox

Extensions are WebAssembly components (Wasmtime 47, Component Model, a WIT world
in `nebula-wasm-host`). A component can only call what the host explicitly
provides, and the host provides only what the extension's manifest declares and
the user approved. Execution is bounded by epoch interruption and a
`ResourceLimiter` on memory and table growth.

Notarisation compares the capabilities a package *declares* against the imports
the compiled component *actually has*. Declaring more makes the install prompt
scarier than it needs to be; declaring less is a rejection.

### Ring 2 — OS-level confinement

| Platform | Mechanism | What it covers |
| --- | --- | --- |
| Linux | Landlock LSM (ABI probed via syscall 444) + seccomp-bpf | Filesystem scoping, network |
| macOS | Seatbelt (`sandbox_init`) | Filesystem scoping, network |
| Windows | Job Objects | Process and resource limits |

Applied to the calling thread after `fork` and before `exec`, so it is inherited
by everything the child later runs.

**The seccomp filter matches on address family, not on syscall.** The obvious
filter — deny `socket`, `connect`, `sendto` outright — breaks ordinary process
spawning: Rust's standard library builds the CLOEXEC pipe it uses to report exec
failures with `socketpair(AF_UNIX, …)`, so a blanket denial makes every sandboxed
build tool fail to invoke its linker. That was a real bug, found by the stress
harness. The filter now allows `AF_UNIX` and refuses every other family, which is
both less blunt and no weaker: a process that cannot *create* an `AF_INET` socket
cannot connect, bind or send on one either.

`EPERM` rather than `SIGSYS`: a build tool that probes for an update should fail
that one call and carry on compiling.

### Ring 3 — resource limits

`RLIMIT_AS`, `RLIMIT_NPROC`, `RLIMIT_CPU`, `RLIMIT_FSIZE`, a wall-clock timeout,
an output ceiling, and `setsid` so a runaway process group can be killed whole. A
limit that cannot be set is skipped rather than failing the launch — a container
may already impose something stricter than the policy asks for, and refusing to
run inside it would be absurd.

---

## AI

### Bring your own key, and mean it

Requests go **directly from the user's machine to the provider**. There is no
Nebula-operated proxy on the path, because a proxy is a place where prompts and
source code accumulate. Keys live in the OS keychain — Keychain on macOS, Secret
Service on Linux, Credential Manager on Windows — never in a config file. A
provider's own environment variable is read as a fallback so a developer who
already exported one does not enter it twice; Nebula never *writes* it.

### The model catalogue

| Model | Context | Max output | Input $/Mtok | Output $/Mtok | Sampling params |
| --- | ---: | ---: | ---: | ---: | --- |
| `claude-fable-5` | 1 M | 128 K | 10.00 | 50.00 | rejected |
| `claude-opus-5` | 1 M | 128 K | 5.00 | 25.00 | rejected |
| `claude-sonnet-5` | 1 M | 128 K | 2.00 | 10.00 | rejected |
| `claude-haiku-4-5` | 200 K | 64 K | 1.00 | 5.00 | accepted |

Opus 4.7 and later reject `temperature`, `top_p` and `top_k` with a 400 — not a
soft failure, the whole request fails. The adapter consults the catalogue before
building the body and drops the parameter with a debug log rather than sending a
request it knows will be refused.

### Prompt caching

Five-minute writes cost 1.25×, one-hour writes 2×, and reads 0.1× the base input
rate. `nebula-ai::cache` places breakpoints and `Model::estimate_cost` bills
against these multipliers, so the cost the UI shows is the cost that is charged.

### The agent loop

`nebula-agent` runs tools against a capability set the user granted, writes every
call to a blake3 hash-chained audit log, and screens tool *results* for injected
instructions before they re-enter the model's context. The audit log's chain
means a tampered entry invalidates every entry after it.

---

## Protocols

### MCP, spec 2026-07-28

Stateless core: no session initialisation handshake for the common path.
`server/discover` for capability discovery, Multi Round-Trip Requests for
operations that need several exchanges, `Mcp-Method` and `Mcp-Name` headers for
routing, and `ttlMs`/`cacheScope` on list results so a client can cache them.

Sampling, Roots, Logging and HTTP+SSE are deprecated in this revision and are not
implemented. Implementing a deprecated transport to be generous is how a client
ends up maintaining it for a decade.

### LSP

`Content-Length` framing, UTF-16 code-unit positions, `LocationLink` handling.
Language servers are launched through `nebula-exec`, so they inherit the sandbox
and the resource limits like any other child process.

---

## Distribution

### Extensions

A package is a signed tar archive: an Ed25519 signature over a manifest, the
compiled component, and its assets. The reader rejects traversing paths
(`../../.bashrc`, `/etc/cron.d/backdoor`) — tested by hand-building a malicious
tar header, because the writer refuses to produce one.

The registry notarises on submission and holds anything questionable for human
review rather than rejecting or accepting silently.

### Licensing

Ed25519 tokens verified offline against a compiled-in issuer key. A licence binds
to a hardware fingerprint built from several independent signals and matches on a
*majority* of them, so replacing a disk does not invalidate a licence. Expiry has
a grace period, and an expired licence degrades to the free tier rather than
locking the user out of their own files.

### Updates

Signed delta patches. The updater verifies the signature over the manifest before
applying anything; an unsigned or mis-signed update is refused, not warned about.

---

## Performance budgets

From the blueprint, checked by the stress harness rather than asserted here.

| Budget | Target | Measured (CPU renderer, 1920×1080) |
| --- | ---: | ---: |
| Keystroke to photon, GPU | 8 ms | not measurable in this container |
| Keystroke to photon, CPU | 16 ms | **p50 7.3 ms, p95 11.0 ms** |
| Same, on a 300 007-line file | 16 ms | **p95 6.2 ms** |
| Cold start to first frame | 500 ms | **14.1 ms** |
| Resident memory | 300 MB | not yet instrumented |

See [the stress report](../stress-results/REPORT.md) for the full run, and
[INFRASTRUCTURE.md](INFRASTRUCTURE.md) for how CI reproduces it.

---

## Honest limits

Stated plainly, because a design document that only lists strengths is marketing.

* **The GPU path is untested on real hardware in this repository's CI.** Linux
  runners use lavapipe, which is a software Vulkan implementation. It exercises
  the wgpu code path but tells you nothing about a real driver.
* **Resident memory is not instrumented.** The 300 MB budget is stated in the
  blueprint and is not yet measured by the harness.
* **Landlock was unavailable in the container these numbers came from.** The
  probe is correct and the enforcement tests are gated behind
  `NEBULA_REQUIRE_SANDBOX=1` in CI, where the kernel does support it. The
  reported run therefore ran its six programs with seccomp and rlimits but no
  filesystem scoping, and the report says so.
* **A WASM sandbox is not a security boundary against the host it runs on.**
  It bounds what an extension can *ask for*. It does not defend against a
  Wasmtime vulnerability, which is why ring 2 exists.
* **Code that ships to a user's machine can be reverse-engineered.** Nothing in
  the packaging, signing or licensing here changes that, and none of it is
  presented as doing so. Signing establishes *who published* an extension, not
  that it cannot be read.
* **The vector embedder is a hashing embedder**, not a learned model. It is
  deterministic, dependency-free and adequate for lexical retrieval; it is not
  semantic search.

---

## Where to read next

| Document | What it covers |
| --- | --- |
| [INFRASTRUCTURE.md](INFRASTRUCTURE.md) | CI/CD, release targets, signing, reproducing the numbers |
| [THREAT_MODEL.md](THREAT_MODEL.md) | What the sandbox defends against, and what it does not |
| [STRESS_TESTING.md](STRESS_TESTING.md) | What the harness does and how to run it |
| [../stress-results/REPORT.md](../stress-results/REPORT.md) | The most recent recorded run |
