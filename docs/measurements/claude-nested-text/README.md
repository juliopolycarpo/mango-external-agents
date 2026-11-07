# Claude nested-text allocation receipt

The reducer now appends only the Unicode-safe prefix that fits its nested-text carry.
An oversized first forwarded block no longer leaves an input-sized allocation behind a
bounded visible string. This is a synthetic allocation comparison of the public reducer,
not a vendor capture or a whole-application measurement.

Sources: [issue #261](https://github.com/juliopolycarpo/mango-external-agents/issues/261),
[finish-retention issue #252](https://github.com/juliopolycarpo/mango-external-agents/issues/252),
and [PR #221](https://github.com/juliopolycarpo/mango-external-agents/pull/221).

## Decision and scope

**Accepted:** all original gates passed in 24 profiles, with no failed, excluded or retried
attempts. A separate 24-profile validation of the portable reproducer passed and produced
identical allocation metrics. The batches are retained separately and are not pooled.

The baseline is signed `1c07cd6f52098da30ae858465caa40b5f38b498f`, the exact original #221
rebase onto main `f181ea8c786cd37fbc8a092753b5e42c642cf47d`. It already includes finished
buffer cleanup. The candidate is signed `045be3f73e0c245aa45adf320a1b317f2d40e289`, which
adds prefix-only appending and regression coverage. This comparison therefore isolates
active-buffer allocation from the separate finish-cleanup correction.

Before the corrections, separate executed current-main cancel, abort and result tests each
failed with retained child buffers. The oversized capacity test then failed on rebased #221:
its visible text was bounded, but its retained String capacity was 2,097,152 bytes. The fixed
source passes all 44 reducer tests, including ASCII/two-byte/four-byte capacity, empty/exact
boundary markers, parent isolation, tool-result removal, closure events and late-record cases.
The container is a `BTreeMap`; clearing it releases nodes and values. There is no retained
hash-table-bucket premise.

The [regression proof index](regression/red-proof.json) records source hashes and intended
failure reasons. Executed failures are retained for [cancel](regression/pr-221-cancel-red.txt),
[abort](regression/pr-221-abort-red.txt), [result](regression/pr-221-result-red.txt) and
[oversized capacity](regression/pr-261-capacity-red.txt), with matching `.patch` files beside
each log. The finish patches apply to main `f181ea8`; the capacity patch applies to baseline
`1c07cd6`. Run the named single test after applying its patch. The
[fixed reducer suite](regression/reducer-green.txt) passes all 44 tests. An earlier
[empty-record test setup failure](regression/empty-record-test-setup-failure.txt) is retained
but is not bug proof: the protocol correctly ignores empty records, so the final boundary
case exercises the append helper directly.
Raw regression patches and logs preserve their original whitespace; the artifact directory's
attributes exempt only those generated files from whitespace-error checks.

## Allocation results

Primary metric: live requested heap bytes owned by the reducer after all scenario records
and returned-event drops, while the reducer remains active. Both pairs produced the exact
values below; the observed within-scenario range was zero. Two deterministic allocation
pairs do not establish timing percentiles or statistical confidence intervals.

| Scenario                             | Baseline active bytes | Candidate active bytes | Paired change |
| ------------------------------------ | --------------------: | ---------------------: | ------------: |
| Ordinary 256-byte ASCII block        |                   992 |                    992 |            0% |
| Eight ordinary mixed-Unicode blocks  |                 4,064 |                  4,064 |            0% |
| First 2 MiB ASCII block              |             2,097,888 |                  8,928 |       −99.57% |
| First 2 MiB four-byte Unicode block  |             2,097,888 |                 33,504 |       −98.40% |
| 256-byte growth, then 2 MiB ASCII    |             2,098,145 |                  8,928 |       −99.57% |
| Full first block, then ignored 2 MiB |             2,097,888 |                  8,928 |       −99.57% |

Every oversized case passed the predeclared reduction of at least 90% in each pair.
Ordinary ASCII retained 17 allocation events, 4,346 total requested bytes and 2,212 peak
requested bytes on both sides. Ordinary Unicode retained 56 events, 24,574 total requested
bytes and 6,699 peak requested bytes on both sides. Neither ordinary gate regressed.
The first oversized ASCII case kept 17 events on both sides, while total requested bytes
fell from 2,109,178 to 20,218 and peak requested bytes from 2,107,044 to 18,084.

Every profile had zero tracked bytes/blocks after finish and again after reducer drop.
Both binaries produced identical serialized-event digests, lengths and counts in all six
correctness workloads. Individual samples, including all secondary metrics, are in
[raw samples](raw/raw-samples.json) and the [evaluation](raw/summary.json).

## Plan and validity

The [plan](plan.json) was registered on issue #261 before collection. Its SHA-256 is
`5dc521a3d84b75dcfd523cf6b4dacd1570e1224ceef6241d0d5cb8b116fa8431`.
There are two pairs per scenario: baseline then candidate in pair 1, candidate then baseline
in pair 2. Each profile runs in a fresh process with one unprofiled complete warm-up/cancel/drop.
Input records are prepared and parsed before profiling, then borrowed by the reducer.
Returned events are dropped before the active snapshot. No serialization or output logging
is included in the profiled operation. Local builds/tests were stopped during collection.

The original profile interval was 2026-11-07 07:59:11.837107–07:59:13.988947 UTC. All seven
setup commands and all 24 profiles exited successfully; the 30-minute setup/trial budget and
60-second per-process timeout were met. No retry or exclusion rule was invoked.
The [command audit](raw/attempts.jsonl) records each command, cwd, timestamps, exit status and
stdout/stderr hashes. Raw DHAT totals were crosschecked against every sample's total bytes,
allocation blocks and end bytes. The [crosschecks](crosschecks.json) also compare both batches.

Execution: managed Linux 6.18.57 x86_64, four-CPU cgroup quota and 16 GiB cgroup memory limit;
Rust 1.99.0 `b940084d7`, Cargo 1.99.0 and LLVM 23.1.1. Each side has an independent writable
release target with `debug = 1`. The Cargo registry cache was shared and warm; OS caches were
uncontrolled/warm. No machine-cold or native Windows/macOS performance result is claimed.
See the [environment and graph verification](raw/environment.json) and
[artifact identities](raw/identities.json).

All original raw files, including the large DHAT profiles, full dependency metadata and every
command's stdout/stderr, are retained byte-for-byte in [raw-evidence.tar.gz](raw-evidence.tar.gz).
The [archive index](archive-index.json) records each member's SHA-256 and size. Summaries,
audits, correctness output and identities also remain readable as individual files. To restore
the full raw layout beside this receipt, run `tar -xzf raw-evidence.tar.gz` from this directory.

Both SDK manifests and SDK lockfiles are byte-identical. The standalone probe lock is
`d4ff078ba12e7ca0e0afb381904974da9882b4f63cb0d582d9a32e47d2aac5fa` on both sides.
Resolved graphs match after normalizing only SDK/probe source and output paths; metadata
verifies the two SDK crates actually resolve from the intended source tree. DHAT, serde_json
and sha2 are dependencies of this standalone probe only, not additions to the SDK.

The frozen [probe](probe.rs) SHA-256 is
`8de148885026e5b3198961eb6af72bcf3e79e85410d463cf6cc834cc992cd241`.
The original [collection harness](harness-recorded.py) SHA-256 is
`6ecd61296de0f23953984c1810bc775ca8d208256190031aebf64432af9d5fce`.
The exact generated manifests are preserved as [baseline](base-Cargo.toml.txt) and
[candidate](variant-Cargo.toml.txt) text, so formatting cannot change their recorded bytes.

## Reproduction

Create clean, independent worktrees at the baseline and candidate SHAs above. With Rust
1.99.0, Python 3 and Cargo available, run:

```sh
python3 docs/measurements/claude-nested-text/reproduce.py \
  --baseline /path/to/baseline \
  --candidate /path/to/candidate \
  --output /path/to/fresh-output
```

The [portable reproducer](reproduce.py) checks the exact clean source SHAs, uses the committed
[probe lockfile](probe-Cargo.lock), creates separate build directories, verifies correctness,
and writes every attempt and profile to the fresh output directory. `--verify-only` checks
source identities and stages the plan without building or profiling. An existing output
directory is refused so earlier attempts are preserved. The portable script is separate from
the frozen collection harness and records its own hash and derived plan before measurement.

Its actual full validation is preserved under [reproduction](reproduction/summary.json), with
its [derived plan](reproduction/plan.json), [identities](reproduction/identities.json),
[command audit](reproduction/attempts.jsonl), raw profiles and logs. It uses the same probe
and source SHAs, but its different probe/output paths and script identity remain explicit.

## Limits

These are allocator event counts and requested live/peak heap bytes for a synthetic public
reducer workload. Caller-owned parsed input and JSON parsing are excluded. Peak allocation
is not process RSS. This does not measure CPU time, latency, a full Claude session, real vendor
traffic, many simultaneous children or MangoStudio adoption. SDK source remains unreleased
0.8.0; no package publication is part of this change.
