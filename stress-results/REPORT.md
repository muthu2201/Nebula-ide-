# Nebula end-to-end stress run

**PASSED** — 2026-08-01T04:36:27Z on linux x86_64, 4 hardware threads, CPU (software) renderer.

Renderer selection: using the software backend because the GPU was unavailable: no usable GPU adapter: No suitable graphics adapter found; noop support not compiled in, vulkan drivers/libraries could not be loaded, metal support not compiled in, dx12 support not compiled in, gl drivers/libraries could not be loaded, webgpu support not compiled in

Sandbox: Linux (Landlock unavailable; kernel older than 5.13 or disabled) (not available on this kernel; programs ran unconfined)

Fixture: 16 files, 300322 lines, 6 runnable programs.

Total time: 50.33s

## Phases

### cold-start — ok (14.10ms)

CPU (software) renderer

| Measurement | Time | Budget | |
| --- | ---: | ---: | :--- |
| time-to-first-frame | 14.10ms | 500.00ms | within |

### editing — ok (38.00s)

5000 keystrokes, each followed by a full frame

| Measurement | Time | Budget | |
| --- | ---: | ---: | :--- |
| keystroke-to-photon-p50 | 7.32ms | 16.00ms | within |
| keystroke-to-photon-p95 | 11.00ms | 16.00ms | within |
| keystroke-to-photon-p99 | 12.55ms | — |  |
| keystroke-to-photon-worst | 18.66ms | — |  |
| 64-cursor-edit | 5.66ms | — |  |
| undo-507-steps | 16.40ms | — |  |

### large-file — ok (5.80s)

300007 lines, 7748759 characters

| Measurement | Time | Budget | |
| --- | ---: | ---: | :--- |
| open | 1.26s | — |  |
| first-frame | 11.03ms | — |  |
| scroll-frame-p95 | 7.87ms | 16.00ms | within |
| scroll-frame-worst | 9.13ms | — |  |
| keystroke-to-photon-p95 | 6.16ms | 16.00ms | within |
| keystroke-to-photon-worst | 8.13ms | — |  |

### indexing — ok (5.67s)

15 files

10 ranked files, 1007 bytes rendered

1007 matches for `pub fn`

15 vectors indexed

| Measurement | Time | Budget | |
| --- | ---: | ---: | :--- |
| walk | 1.05ms | — |  |
| repo-map | 3.38s | — |  |
| content-search | 2.36ms | — |  |
| embed | 2.29s | — |  |
| hnsw-build | 110.47µs | — |  |
| hnsw-query | 16.94µs | — |  |

### programs — ok (625.16ms)

6 of 6 programs built and ran

| Measurement | Time | Budget | |
| --- | ---: | ---: | :--- |
| rust-total | 304.06ms | — |  |
| python-total | 62.27ms | — |  |
| javascript-total | 67.49ms | — |  |
| go-total | 73.48ms | — |  |
| c-total | 94.30ms | — |  |
| bash-total | 23.18ms | — |  |

### durability — ok (196.96ms)

2 files saved and verified, 11 re-parsed

## Programs

| Language | Command | Build | Run | Result | Output |
| --- | --- | ---: | ---: | --- | --- |
| rust | `./nebula-fixture-rust` | 277.74ms | 26.32ms | ok | area=78.54 mean=3.00 stddev=1.41 |
| python | `python3 scripts/analyse.py` | — | 62.27ms | ok | records=5 total=150 mean=30.0 |
| javascript | `node web/index.js` | — | 67.49ms | ok | primes<50=15 sum=328 |
| go | `go run cmd/report/main.go` | — | 73.48ms | ok | lines=4 words=21 longest=comprehensive |
| c | `./nebula-fixture-sieve` | 69.97ms | 24.33ms | ok | primes below 1000: 168 |
| bash | `sh scripts/summary.sh` | — | 23.18ms | ok | sources=12 |

