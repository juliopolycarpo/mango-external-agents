# Current-main data-URL allocation qualification

The allocation experiment passes against current main. The [2026-11-04 receipt](README.md) and
its raw samples remain historical evidence; this is a separate comparison on 2026-11-07.
Tracked in [GitHub issue #249](https://github.com/juliopolycarpo/mango-external-agents/issues/249).

## Plan and identity

The [plan](requalification-2026-11-07/plan.json) was recorded before collection. Baseline:
`f5f39b2cefb4ac4fe413bcb02bb8e1e0a3c419e4`; candidate:
`c89d479af1c54cc937f611a79807390402ebeecc`. Their exact extracted helper hashes are
`df513901f3e71388621be7df2c57406b566021effeeb3fa709b638caa45d3621` and
`4b82af11087f5c4e4e730a198b0b74cc77bab81a9ece856f1d06940fc73315de`, respectively: both match
the historical experiment. SDK manifests and `Cargo.lock` are unchanged by this PR.

The unchanged Rust probe uses DHAT 0.3.3, the same locked probe dependencies and Cargo's default
release profile. Both exact helper modules are compiled into one probe; each measured call gets
a fresh single-thread process, one unprofiled warm-up and an input allocated before profiling.
The output remains live until the snapshot. The Python harness now retains commands, UTC start/end
times and exits for setup, equality and every profile. Its [patch](requalification-2026-11-07/harness.patch)
is recorded separately from the unchanged measured Rust sources.

Execution: Linux 6.18.57, x86_64 GNU, AMD EPYC 9V74, five affinity CPUs, four CPUs of cgroup quota
and 16 GiB memory limit. Rust 1.99.0 `b940084d7`, LLVM 23.1.1. SDK dependency caches were warm;
the fresh probe build downloaded some profiling dependencies. Build output was fresh, application
state was a fresh process per sample, and OS cache state was uncontrolled. No other local builds
or tests ran during profiles; all workers settled. Build/setup is outside the allocation metric.

Measured probe SHA-256: `39d7432974723519de6233ff48faef3345b6fb50124d1e8a7275930829cb6102`.
The [artifact identity](requalification-2026-11-07/artifact.json) records probe, lock and harness
hashes. [Build log](requalification-2026-11-07/build.log),
[equality log](requalification-2026-11-07/equality.log),
[all attempts](requalification-2026-11-07/attempts.jsonl) and
[raw samples](requalification-2026-11-07/raw.csv) are retained. All 28 profiles exited successfully;
there were no failed, excluded or retried samples.

## Allocation results

Each size has two pairs, ordered baseline/candidate then candidate/baseline. Both pairs returned
identical counts: observed spread was zero for every reported value.

| Input bytes | Allocation/reallocation events, base → candidate | Requested bytes, base → candidate | Maximum profiled live bytes, base → candidate |
| ----------: | -----------------------------------------------: | --------------------------------: | --------------------------------------------: |
|           0 |                                            1 → 1 |                           26 → 22 |                                       26 → 22 |
|   1 / 2 / 3 |                                            2 → 1 |                           30 → 26 |                                       30 → 26 |
|       1,024 |                                            3 → 1 |                     2,784 → 1,390 |                                 2,758 → 1,390 |
|      65,536 |                                            3 → 1 |                  174,816 → 87,406 |                              174,790 → 87,406 |
|   2,097,152 |                                            3 → 1 |             5,592,456 → 2,796,226 |                         5,592,430 → 2,796,226 |

Every nonempty scenario removes at least one allocation/reallocation event and the complete
intermediate Base64 payload; empty input does not regress. At 2 MiB, the removed payload and
profiled-live reduction are 2,796,204 bytes. The requested-byte reduction also includes the old
small prefix allocation. These are per-call allocations and profiled heap bytes, excluding the
already allocated input and other process memory. No latency, throughput, process RSS, simultaneous
attachment peak, binary size, distribution size or native-platform performance claim is made.

All 5,155 exact output comparisons pass. Independent RFC, high-bit, MIME/padding, maximum attachment,
preflight/refusal, replay and version-floor coverage passes in the full Codex crate check:
613 tests, one existing skip. Full repository/native/review delivery gates remain separate.

## Dependency graphs

The standalone Codex normal graph is identical on both sources: 42 lines, normalized SHA-256
`5b1f4990d8a4269963439e8657b36ef40fa16334631e1d5c8300616e5397a0d8`.

MangoStudio at `ebc4ba3f66c4ac940a66e9fbf5eb67542539932c` still pins SDK 0.7.1. An isolated
prospective consumer fixture changes its six SDK version declarations to 0.8.0 and resolves the
four path-patched SDK packages once. Both comparisons then use the same locked fixture and
`--no-default-features -p mangostudio-runtime`. All four SDK packages are verified to resolve to
the intended source tree on each side; unused patches cannot satisfy the check.

That prospective runtime graph is identical: 593 lines, normalized SHA-256
`3474f2d5c34b69f5afeca7603fbb524100b8f8f4319958fc41853480261b5350`.
Only SDK and consumer source-root paths are normalized. The
[fixture patch](requalification-2026-11-07/graphs/consumer-fixture.patch),
[graph attempts and commands](requalification-2026-11-07/graphs/attempts.json), raw outputs and
[summary](requalification-2026-11-07/graphs/summary.json) are retained. This is graph-only
qualification of prospective 0.8 input: the actual consumer checkout is untouched, no runtime
binary was built, and no consumer adoption or publication occurred.

## Reproduce

From the SDK repository root with the pinned compiler activated:

```sh
python3 docs/measurements/codex-data-url/reproduce.py \
  --base f5f39b2cefb4ac4fe413bcb02bb8e1e0a3c419e4 \
  --candidate c89d479af1c54cc937f611a79807390402ebeecc
```

The script prints its retained directory before building, so a failed setup is also discoverable.
Profiling dependencies stay outside the SDK workspace. The dated
[summary](requalification-2026-11-07/summary.json) records the accepted allocation experiment;
it does not itself record PR landing or product delivery.
