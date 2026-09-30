# Benchmarks

The workspace carries a small set of benchmarks for the hot paths a change to framing, the
notification pipeline or event accounting can move. They exist so a pull request that claims a
speed-up can show a repeatable Base / Variant / Delta receipt instead of a one-off probe.

They are **not a CI gate**. No job runs them and no threshold fails on them: numbers from a shared
machine are evidence for a review, not a contract. `cargo nextest`, `cargo test` and
`scripts/check.sh` never build a bench target to run it, and `cargo clippy --all-targets` compiles
them only so they stay lint-clean.

## Run them

```sh
scripts/bench.sh                          # everything; prints the environment first
scripts/bench.sh chunk-4KiB               # only cases whose name contains the argument
BENCH_SAMPLES=25 scripts/bench.sh         # more samples per case (default 15)
cargo bench -p mango-external-agents --bench framing   # one binary, without the environment block
```

Run benches with `cargo bench` or `scripts/bench.sh`, never `cargo run`: a debug build prints a
warning in its header and its numbers are not comparable. Arguments after `--` that do not start
with `-` filter cases by substring; flags such as `--bench` are ignored.

| Binary                                | Cases                                                                                                                                                                                                                                                                                                                                                                                         |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mango-external-agents` / `framing`   | `LineStream`: a 128 KiB line at 16 KiB chunks; a 1 MiB line at 4 KiB, 16 KiB and 1 MiB chunks; 15-byte records; an invalid-UTF-8 line (the lossy repair path); the captured fixture transcripts replayed at 4 KiB and 16 KiB chunks                                                                                                                                                           |
| `mango-external-agents` / `events`    | `EventSink::emit` plus drain for 1 KiB deltas (ASCII, JSON escapes, non-ASCII, characters the sanitiser strips) and a rich activity; the same events serialized to a counting writer and to a string; `normalize::sanitize_field` alone                                                                                                                                                       |
| `mango-external-agents` / `copies`    | `Client` dispatch of prebuilt frames through a scripted link (1 KiB notification, 500 KB diff notification, 500 KB and error responses); `stdio::open` link sends of 1 KiB and 1 MiB messages into a counting sink                                                                                                                                                                            |
| `mango-agent-acp` / `reducer`         | `Reducer::update_at` on tool-call frames: ten 100 KiB diffs as a new call, as an update and as a completion; a 1 MiB image ahead of a text block; a 1 MiB text body; and the frames most agents send (two 2 KiB diffs, a short text)                                                                                                                                                          |
| `mango-agent-codex` / `pipeline`      | Per stage and end to end: raw line to JSON, `Notification::parse`, `TurnReducer::reduce`, emit, drain, serialize. 1 KiB text deltas; 500 KB patch updates (throttled and emitted); a file change started and completed; command output inside the throttle window; every captured Codex transcript                                                                                            |
| `hub-host` / `retry`                  | `Supervisor::run` re-sending an 8 MiB, 2 MiB, 1 MiB or attachment-free request after `NotSubmitted` answers (1 or 3 re-sends), on a paused clock so backoff costs nothing; the process's peak resident set is printed after the cases                                                                                                                                                         |
| `mango-agent-claude` / `tool_results` | `TurnReducer::reduce` closing a call: a `tool_result` record with a string payload (1 MiB, 32 KiB) or an array of text blocks (1 MiB in one block and in 100 blocks, 12 KiB and 32 KiB in one block, 6 KiB and 600 bytes in three); starting a `Write` (16 KiB, 200 KiB) or an `Edit` (two 2 KiB or two 100-byte strings) call; 1000 subagent text blocks of 1 KiB forwarded under one `Task` |

Reading the output: one line per case with the median, minimum, maximum, coefficient of variation
(CV) and a per-unit cost, then a `samples_ms:` line with every raw sample in run order. The raw
samples are what a receipt quotes.

Two things to know when reading them:

- The byte counter behind `turn_buffer_bytes` is private. The `events/emit+drain/*` cases are the
  authoritative measurement of it, since a change to it moves them. The
  `events/serialize-count/*` cases are a stand-in: the same counting-writer serialization the
  counter performs today, over already-queued events. They stay comparable only while the counter
  keeps that shape.
- `stage/*` cases time one step and drop what it returns inside the timing, so a step that
  allocates is charged for freeing too. The `pipeline/*` cases run every step, so they will not
  equal the sum of the stages: a step's output is warm in cache for the next one.

## Why this harness

The runner is a small `std`-only module (`benches/support/mod.rs` in each crate that has benches)
instead of criterion or divan:

- No dependency enters the lockfile, so `cargo deny`, the TLS check and the MSRV lane are
  untouched, and nothing enters the published dependency graph.
- It prints raw samples, which a review can recompute from; it makes no statistical claim.
- The library crates, and the `hub-host` reference host, set `bench = false` on their `[lib]` target
  so `cargo bench` skips their unit tests. The Codex, ACP, Claude and `hub-host` copies of the
  runner are byte-identical to the core one because a published crate cannot package a file from
  another crate; change every copy together. `scripts/check-bench-runner.sh` lists them and fails
  `scripts/check.sh` and CI's Policy lane when they differ, `scripts/test-bench-runner.sh` fails if
  a tracked copy is missing from that list, and `scripts/bench.sh` runs the check first. The
  runner's own tests, such as the `BENCH_SAMPLES` validation, run under `cargo nextest` through
  `examples/hub-host/tests/bench_runner.rs`, since a bench target cannot run `#[test]`s.

Allocation counts are not measured. A counting global allocator needs `unsafe`, which the
workspace forbids. To find allocations, use a profiler on the bench binary
(`target/release/deps/<name>-<hash>`), for example `valgrind --tool=dhat` or `heaptrack`, outside
the repository.

## Record a Base / Variant / Delta receipt

Use this in a pull request that claims a speed-up or a cost, and in one that closes with "no
measurable gain".

1. **Pick the cases the change can move**, from the table above, and say which. A change to
   `LineStream` reports `framing/*`; one to the Codex notification decoder reports
   `codex/stage/wire-parse/*` and `codex/pipeline/*`; one to event accounting reports
   `events/emit+drain/*`. Add a case if none covers the code (see below).
2. **Prepare Base and Variant in two worktrees** of the same machine, so the build cache and the
   toolchain cannot differ: `git worktree add ../base origin/main` for Base, the branch for
   Variant. Build both first, so compile time is not on the clock.
3. **Quiet the machine.** Close other builds and note `load` from the environment block. On a
   shared machine, alternate the runs (Base, Variant, Base, Variant, ...) rather than running all
   of one first, so drift hits both sides.
4. **Run `scripts/bench.sh` in each worktree at least 5 times** with the same `BENCH_SAMPLES`
   (25 is a good start for anything that varies more than 5%). Keep each run's output.
5. **Compute** each side's median of the per-run medians, and

   ```text
   Delta = (Variant - Base) / Base
   ```

   Report the spread (min to max of the per-run medians) next to it. A Delta inside the spread,
   or inside the CV of the noisier side, is noise, and the pull request says so.
6. **Paste the table and the environment into the pull request**, with the raw `samples_ms:` lines
   of at least one run per side (a collapsed `<details>` block is fine):

   ```text
   | Case                            | Base (median ms) | Variant (median ms) | Delta  | Base range     | Variant range  |
   | ------------------------------- | ---------------- | ------------------- | ------ | -------------- | -------------- |
   | framing/line-1MiB/chunk-4KiB    | <median>         | <median>            | <±%>   | <min - max>    | <min - max>    |
   ```

   The environment block from `scripts/bench.sh` (commit, lockfile digest, toolchain, CPU, load,
   samples) goes above it. Base's commit is the merge base, not a moving `main`.

A change whose Delta stays inside the noise, or which only helps an input the fixtures do not
produce, is closed or dropped with these numbers, not merged on an assumed gain.

## Add a case

- In a bench binary, `bench.run(name, Unit::new(count, "label"), setup, routine)` runs
  `routine(setup())` once per sample. Only `routine` is timed. Build inputs in `setup`, and do at
  least a millisecond of work per call (loop over a few hundred events) so timer and scheduler
  noise is small next to it.
- Make the routine assert what it should have done (an `Emit`, a line count). A bench that quietly
  times the `Ignore` path measures nothing.
- Build a fresh reducer or sink per sample. Reducers keep per-message state and a sink refuses
  events once it hits its budget, so a shared one changes the workload as it runs.
- A crate without benches yet gets its own `benches/` directory with a copy of `support/mod.rs`
  (listed in `scripts/check-bench-runner.sh`; `examples/hub-host` is the unpublished example), a `[[bench]]` entry with
  `harness = false` and `required-features` for anything feature-gated, and no new dependency that
  is not already in `Cargo.lock`. Run `cargo clippy --workspace --all-targets --all-features -- -D
  warnings` afterwards, since that command compiles bench targets.

## Noise on the reference machine

Measured on a WSL2 Xeon E5-2699 v3 (32 threads) shared with other builds, running
`scripts/bench.sh` 10 times in a row at 15 samples per case, twice:

| Batch                                   | Load average | Within a run: CV of the 15 samples | Between runs: CV of a case's medians |
| --------------------------------------- | ------------ | ---------------------------------- | ------------------------------------ |
| Busy (other builds running)             | 3.5 to 10    | median 6.5%, p90 13%, worst 35%    | median 9%, range 4% to 39%           |
| Quieter (the batch in the pull request) | 2.3 to 6.3   | median 2.2%, p90 8.7%, worst 41%   | median 3.5%, range 0.9% to 41%       |

The drift between runs is machine load, not sampling error, so more samples inside one run do not
fix it. A single slow run also drags a case's spread wide (the two 40% ranges are one outlier run
each) while the median of the run medians stays put.

What that means for a claim:

- Large effects are safe to claim on either batch: the 1 MiB line at 4 KiB chunks, about 118 to
  123 ms, moved 1.4% to 6% run to run, so a 2x change is unmistakable.
- An effect under about 10% needs care. Copy removals in the 1 KiB paths, each a few percent of a
  microsecond-scale case, fall here. Interleave Base and Variant runs, take at least 5 runs per
  side, compare the medians of the run medians, and report the spread. If the Delta is inside the
  spread, say so.
- A run's minimum is not steadier than its median (4.9% against 3.5% run to run on the quieter
  batch), so do not switch to the minimum to make a result look cleaner.
