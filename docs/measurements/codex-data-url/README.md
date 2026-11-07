# Codex data-URL allocation receipt

This is the historical 2026-11-04 experiment. See the
[2026-11-07 current-main qualification](requalification-2026-11-07.md) for the rebased source,
fresh samples and prospective 0.8 consumer graph evidence.

Measured 2026-11-04 on Linux 6.18.57, x86_64 GNU, AMD EPYC 9V74 (5 available CPUs),
Rust 1.99.0 `b940084d7`, LLVM 23.1.1. Baseline SDK:
`b0e68ecb74e6604b2b68a2b387a23081055b74b3`. Candidate session source blob:
`7d60002a0a569ea50663d44fd8640cffbdeb9e3f`.

## Plan and scope

Measure heap allocations for **one** private `data_url("image/png", bytes)` call, retaining its
output until the snapshot. Extract the exact baseline and candidate encoder functions into a
standalone release-mode probe with DHAT 0.3.3. Prepare input and warm up once outside profiling.
The probe runs one thread. DHAT is measurement tooling only: no SDK dependency or feature changes.

Accept the allocation change if outputs agree exactly, every nonempty scenario removes at least
one allocation and the full intermediate payload, empty input does not regress, focused attachment
and refusal tests pass, and both standalone and actual consumer graphs stay unchanged.

The predeclared sizes were 0, 1, 2, 3, 1,024, 65,536 and 2,097,152 bytes. Each size has two pairs,
orders baseline/candidate and candidate/baseline, in separate processes. There are no exclusions,
retries or failed profile samples. Pattern byte `i` is `(i * 73 + i / 7) & 255`.

This is a deterministic allocation profile, not a concurrent timing benchmark. Machine load does
not change these counts; pre-run load averages were 0.66 / 0.48 / 0.30. Dependency/build caches were
warm after setup; application state is one fresh process per sample. OS cache was uncontrolled.
No latency, throughput, process RSS, linked executable or distribution-size effect was measured.
The probe uses Cargo's default release profile, not MangoStudio's fat-LTO production profile.

## Results

[Raw samples](raw.csv) retain all 28 calls. Both pairs produced identical results:

| Input bytes | Total allocations/reallocations, base → candidate | Total requested bytes, base → candidate | Maximum live heap bytes, base → candidate |
| ----------: | ------------------------------------------------: | --------------------------------------: | ----------------------------------------: |
|           0 |                                             1 → 1 |                                 26 → 22 |                                   26 → 22 |
|   1 / 2 / 3 |                                             2 → 1 |                                 30 → 26 |                                   30 → 26 |
|       1,024 |                                             3 → 1 |                           2,784 → 1,390 |                             2,758 → 1,390 |
|      65,536 |                                             3 → 1 |                        174,816 → 87,406 |                          174,790 → 87,406 |
|   2,097,152 |                                             3 → 1 |                   5,592,456 → 2,796,226 |                     5,592,430 → 2,796,226 |

DHAT's total blocks include reallocations: the original `format!` can grow its prefix allocation.
Maximum-live bytes cover only allocations made inside the profiled encoder, excluding its already
allocated input and process overhead. At 2 MiB the **intermediate payload** removed is 2,796,204
bytes; total allocated bytes also lose the original small prefix allocation. These are different
metrics, and neither is an RSS measurement or a four-attachment simultaneous-peak claim.

Exact baseline/candidate equality passed for lengths 0 through 1,024, 65,535/65,536/65,537 and
2,097,150/2,097,151/2,097,152, with PNG/JPEG/GIF/WebP and uppercase `IMAGE/PNG`: 5,155 comparisons.
SDK tests retain independent RFC vectors, high-bit inputs, MIME case, all padding residues and
attachment maximum boundaries. Existing preflight, count/byte limits and refusal order are unchanged.

## Production dependency graphs

The SDK manifests and lockfile are unchanged. The standalone Codex normal-dependency graph and
MangoStudio runtime normal-dependency graph were compared for `x86_64-unknown-linux-gnu`, using
`cargo tree --target x86_64-unknown-linux-gnu -e normal --prefix none --format '{p} {f}'`.
The runtime used `--no-default-features -p mangostudio-runtime` at MangoStudio
`69a5d9d30c8d6bf22526d5f2c346b4a946273f16`. In a detached consumer checkout, all four SDK crates were
patched together to the baseline and then the candidate, using Cargo's `--config
patch.crates-io.<crate>.path="<SDK>/crates/<crate>"`; the original consumer checkout was untouched.
No runtime binary was rebuilt for this graph-only comparison.

After replacing only the differing SDK source root with `SDK`, each pair is byte-identical:

| Graph                 | Output lines (includes repeated edges) | SHA-256 on both sides                                              |
| --------------------- | -------------------------------------: | ------------------------------------------------------------------ |
| Standalone Codex      |                                     42 | `563566d922a97d26d9fce904305204892aafca0afbb77da1faab88f3a06b5caa` |
| Full runtime consumer |                                    593 | `9860a72dba8b03df2baa2608c313c44e5bec82e94c1252ec6da0cf4dda6cb5ac` |

Codex adds no Base64 crate. The consumer already retains Base64 0.22.1 through other paths and
0.23.1 elsewhere; this patch claims no dependency removal or binary saving.

## Reproduction and identities

From the repository root, with the pinned Rust compiler and Cargo on PATH:

```sh
python3 docs/measurements/codex-data-url/reproduce.py --candidate HEAD
```

The script keeps generated sources, exact revisions/hashes, the locked probe, build/equality logs,
raw attempts and CSV in its printed temporary directory. The measurement dependencies stay outside
the SDK workspace. Supply another candidate ref only when intentionally starting a new comparison.

| Artifact                | SHA-256                                                            |
| ----------------------- | ------------------------------------------------------------------ |
| SDK Cargo.lock          | `167b77e158e0069d8b8a0008edfb78cd8a25e6c543ff6304159c4fd130fd3f52` |
| Probe Cargo.lock        | `8e194a89da207da00f782cab8bd39c85dc7bdef8ad9851bac397dcc75ebc1c3f` |
| Baseline helper source  | `df513901f3e71388621be7df2c57406b566021effeeb3fa709b638caa45d3621` |
| Candidate helper source | `4b82af11087f5c4e4e730a198b0b74cc77bab81a9ece856f1d06940fc73315de` |
| Measured Linux probe    | `83752fb0f26f8e70fb1c91ab2cbdad17af9bcdb32dfa4d9950e80f71f8a4c3b8` |

## Disposition and remaining checks

Accept the bounded allocation change for review: exact output equality and allocation reduction are
measured, and dependency graphs do not change. Locally, 354 Codex unit tests, 158 captured replays
(one ignored), 21 version-floor tests and all-target Codex Clippy pass. Full repository/feature/MSRV
and native gates remain for coordinated wave validation. There is no new authenticated vendor
smoke, timing, native performance, binary or archive qualification. No package publication or
consumer adoption occurred.
