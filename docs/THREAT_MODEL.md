# Threat model

What the sandboxing in Nebula defends against, what it does not, and where the
honest limits are. A threat model that only lists what a system stops is not a
threat model.

---

## What is being protected

1. **The user's source code and credentials** from extensions, agent tool calls
   and build commands.
2. **The user's machine** from a compromised or malicious extension.
3. **The user's prompts and code** from anyone other than the model provider they
   chose.
4. **The integrity of what gets installed** — extensions, updates.

---

## Who the adversaries are

| Adversary | Capability | Ring that answers it |
| --- | --- | --- |
| A malicious extension | Publishes a signed package a user installs | 1, 2, 3 |
| A compromised dependency of an extension | Arbitrary WASM within the component | 1, 2, 3 |
| A hostile tool result | Text the agent reads back into its context | Injection screening |
| A malicious repository | Files the editor opens, parses and indexes | Parser and path handling |
| A network attacker | Can see or modify traffic | TLS, signatures |
| A registry compromise | Serves a modified package | Publisher signatures |
| A user's own mistake | Grants a capability they did not understand | Notarisation, prompts |

---

## Ring 1 — the WASM capability sandbox

**Defends against:** an extension reading a file it was not granted, calling a
host function it did not declare, spinning forever, or allocating without bound.

**Mechanism:** Wasmtime 47 Component Model. A component can only call imports the
host supplies, and the host supplies only what the manifest declares and the user
approved. Epoch interruption bounds execution time; a `ResourceLimiter` bounds
memory and table growth.

**Does not defend against:** a vulnerability in Wasmtime itself. That is the
whole reason ring 2 exists. A WASM sandbox bounds what code can *ask for*; it is
not a boundary against the host it runs inside.

**The notarisation check that matters:** the capabilities a package declares are
compared against the imports the compiled component actually has. Declaring more
than you use makes the install prompt scarier than it needs to be; declaring less
than you use is a rejection, not a warning.

---

## Ring 2 — OS confinement

**Defends against:** a process — an extension's helper, an agent tool call, a
build command — reading outside the project, writing outside the project, or
reaching the network.

| Platform | Mechanism | Filesystem | Network |
| --- | --- | --- | --- |
| Linux ≥ 5.13 | Landlock + seccomp-bpf | Enforced | Enforced |
| Linux < 5.13 | seccomp-bpf only | **Not enforced** | Enforced |
| macOS | Seatbelt (`sandbox_init`) | Enforced | Enforced |
| Windows | Job Objects | **Not enforced** | Not enforced |

The gaps are reported, not hidden. `nebula_sandbox::apply` returns an
`Enforcement` value — `Full`, `Partial { missing }` or `Unsupported { reason }` —
so a caller makes a decision about running unconfined instead of finding out
afterwards. `nebula-exec` records it on every `Output`, `nebula doctor` prints it,
and the stress report carries it.

### Why the seccomp filter matches on address family

The obvious network filter denies `socket`, `socketpair`, `connect`, `bind`,
`sendto` and friends outright. It is also wrong, and it took a real stress run to
show why: Rust's standard library builds the CLOEXEC pipe it uses to report exec
failures with `socketpair(AF_UNIX, …)`. Under a blanket denial, every sandboxed
build tool fails to spawn its own linker, and `rustc` dies with
`could not exec the linker "cc": Operation not permitted`.

The filter now allows `AF_UNIX` on `socket` and `socketpair` and returns `EPERM`
for every other family. This is no weaker: a process that cannot *create* an
`AF_INET` socket cannot connect, bind or send on one. It is also more useful —
local Unix-socket IPC, which a language server may legitimately use, keeps
working.

`EPERM` rather than `SIGSYS`, so a tool that probes for an update fails that one
call and carries on compiling rather than dying.

---

## Ring 3 — resource limits

**Defends against:** a fork bomb, a memory exhaustion attack, a runaway build, a
process that fills the disk, and output that fills the editor's memory.

`RLIMIT_AS`, `RLIMIT_NPROC`, `RLIMIT_CPU`, `RLIMIT_FSIZE`, a wall-clock timeout,
an output ceiling, and `setsid` so the whole process group can be killed.

A limit that cannot be raised is skipped rather than failing the launch. A
container may already impose something stricter than the policy asks for, and
`setrlimit` then fails with `EPERM` because raising a hard limit needs privilege.
Refusing to run inside an environment that is *more* restrictive than requested
would make the editor unusable in exactly the places it should work best.

---

## Prompt injection

**The threat:** a tool returns text containing instructions — a file comment, a
web page, a CI log, an issue body — and the model treats them as if the user had
typed them.

**What is done:** `nebula-agent::injection` screens tool *results* before they
re-enter the model's context, and the agent loop keeps user instructions and tool
output in structurally distinct positions.

**What is not claimed:** screening is a mitigation, not a solution. There is no
known complete defence against prompt injection. The real boundary is the
capability set: a tool call that has no filesystem write capability cannot be
talked into writing a file, whatever the text says. Capabilities are the
defence; screening reduces the noise.

---

## The audit log

Every tool call is appended to a blake3 hash-chained log: each entry hashes the
previous entry's hash, so tampering with one entry invalidates every entry after
it. The log records what was called, with what arguments, what capability
permitted it, what the sandbox actually enforced, and what came back.

This is for after the fact. It stops nothing; it makes what happened
reconstructible.

---

## Supply chain

| Surface | Control |
| --- | --- |
| Extensions | Ed25519 publisher signatures, namespace ownership at the registry, notarisation on submission |
| Updates | Ed25519 signature over the release manifest, verified before anything is applied |
| Licences | Ed25519, verified offline against a compiled-in issuer key |
| Nebula's own dependencies | `cargo-audit` and `cargo-deny` daily, banned crates, permissive licences only |
| Release artefacts | SHA-256 sums, CycloneDX SBOM, GitHub build-provenance attestation |

**Archive extraction** rejects traversing paths. `../../.bashrc` and
`/etc/cron.d/backdoor` are refused by the reader — tested by hand-building a
malicious 512-byte tar header, because the writer refuses to produce one and a
test that cannot construct the attack is not testing the defence.

---

## Key handling

API keys live in the OS keychain: Keychain Services on macOS, the Secret Service
on Linux, the Credential Manager on Windows. Never in a config file, never in the
project directory, never in a log.

Requests go **directly from the user's machine to the provider**. There is no
Nebula-operated proxy on the path, because a proxy is a place where prompts and
source code accumulate, and a place that can be compromised or subpoenaed.

A provider's own environment variable (`ANTHROPIC_API_KEY` and so on) is read as
a fallback so a developer who already exported one does not enter it twice.
Nebula never writes it.

`ApiKey::redacted()` exists so a key can be shown in a UI or a log line without
being disclosed, and the type has no `Display` that would print it by accident.

---

## What is explicitly not defended against

Stating these is the point of the document.

* **Reverse engineering.** Code that ships to a user's machine can be read.
  Signing establishes *who published* an extension, not that it cannot be
  inspected. No obfuscation is claimed and none is performed.
* **A malicious user on their own machine.** The licence check is an honesty
  mechanism with a grace period, not a DRM system. It is designed to keep an
  honest customer honest and to fail open rather than lock someone out of their
  own files.
* **A compromised OS or kernel.** Every ring assumes the kernel is enforcing what
  it says it is enforcing.
* **Side channels.** Timing, cache and speculative-execution attacks between an
  extension and the host are out of scope.
* **A malicious model provider.** BYOK means the user chose the provider and
  sends their code to it. Nebula removes itself from that path; it does not
  audit what happens at the far end.
* **Denial of service against the editor's own process.** A grammar can be fed a
  pathological file. Parsing is bounded by the visible range and by the deferred
  re-parse, but a sufficiently adversarial file can still make the editor slow.
* **Windows filesystem scoping.** Job Objects bound processes and resources, not
  paths. A Windows user gets rings 1 and 3 in full and ring 2 only partially, and
  `nebula doctor` says so.

---

## Reporting a vulnerability

Security issues should be reported privately rather than as a public issue.
Include the version (`nebula --version`), the platform, and the output of
`nebula doctor` — the last of these says which rings were actually enforcing on
the affected machine, which is usually the first question.
