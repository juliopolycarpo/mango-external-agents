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

| Binary                                | Cases                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `mango-external-agents` / `framing`   | `LineStream`: a 128 KiB line at 16 KiB chunks; a 1 MiB line at 4 KiB, 16 KiB and 1 MiB chunks; 15-byte, 100-byte, 1 KiB and 64 KiB records at 16 KiB chunks; an invalid-UTF-8 line (the lossy repair path); malformed lines of 16 KiB, 64 KiB and 512 KiB with the first invalid byte at the start, middle and end, a multibyte character cut short, one invalid byte in 64 and all invalid, each handing its repaired line back from the routine so that line is freed outside the clock, and a dense-invalid line at the raw cap that repair pushes past the buffered limit (this one frees its repaired string and the stream's buffer inside the clock, as the bench cannot return them); the captured fixture transcripts replayed at 4 KiB and 16 KiB chunks                                                                                                                                               |
| `mango-external-agents` / `events`    | `EventSink::emit` plus drain for 1 KiB deltas (ASCII, JSON escapes, non-ASCII, characters the sanitiser strips), 16- and 64-byte deltas, approval requests and resolutions, a completed activity and a rich activity; the same events serialized to a counting writer and to a string; `normalize::sanitize_field` alone; `normalize::bound_text` at the detail limit over clean ASCII, a late or early non-ASCII character, an escape at, before and after the bound, and ANSI command output; the emit and drain of an activity whose detail has a late non-ASCII character or escape                                                                                                                                                                                                                                                                                                                          |
| `mango-external-agents` / `redact`    | `redact::stderr_text` over a 1 KiB and a 16 KiB stderr tail, 320 KiB per sample: plain diagnostic lines, lines that carry each credential shape, coloured output with control characters throughout, and lines dense in separators and box-drawing characters with nothing to redact; plain lines that end in a non-ASCII character; structured log lines with a quote before nearly every `:`, with nothing to redact and with quoted credentials; JSON inside strings at two and three levels of escaping; a pretty-printed object with credentials in multi-line containers and a YAML sequence; `redact::bounded` over plain lines and lines with unnamed tokens, and four 1 MiB token-pass shapes; a credential name made of one keyword repeated, a URL scheme made of dotted letters, quoted names that never get a value and a run of bare quotes, each at 16 KiB, 120 KB and 1 MiB, one call per sample |
| `mango-external-agents` / `copies`    | `Client` dispatch of prebuilt frames through a scripted link (1 KiB notification, 500 KB diff notification, 500 KB and error responses); `Client` sends of 1 KiB notifications into a discarding sink and 1 KiB requests against a peer that answers each as it is written, the same notifications from eight callers contending for one link, and both queued with `submit_*`; `stdio::open` link sends of 1 KiB and 1 MiB messages into a counting sink, each timed from its call to the end of its write                                                                                                                                                                                                                                                                                                                                                                                                      |
| `mango-agent-acp` / `reducer`         | `Reducer::update_at` on tool-call frames: ten 100 KiB diffs as a new call, as an update and as a completion; a 1 MiB image ahead of a text block; a 1 MiB text body; the frames most agents send (two 2 KiB diffs, a short text); and a 1000- and 10000-call `session/load` replay, through `update_at` and through `Reducer::session_facts`; held updates of one call (2 KiB to 1 MiB)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `mango-agent-codex` / `pipeline`      | Per stage and end to end: raw line to JSON, `Notification::parse`, `TurnReducer::reduce`, emit, drain, serialize. 1 KiB text deltas; 500 KB patch updates (throttled and emitted); a file change started and completed; command output inside the throttle window (1 KiB, 8 KiB and 64 KiB chunks); every captured Codex transcript                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `hub-host` / `retry`                  | `Supervisor::run` re-sending an 8 MiB, 2 MiB, 1 MiB or attachment-free request after `NotSubmitted` answers (1 or 3 re-sends), on a paused clock so backoff costs nothing; the process's peak resident set is printed after the cases                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `mango-agent-claude` / `tool_results` | `TurnReducer::reduce` closing a call: a `tool_result` record with a string payload (1 MiB, 32 KiB) or an array of text blocks (1 MiB in one block and in 100 blocks, 12 KiB and 32 KiB in one block, 6 KiB and 600 bytes in three); starting a `Write` (16 KiB, 200 KiB) or an `Edit` (two 2 KiB or two 100-byte strings) call; 1000 subagent text blocks of 1 KiB forwarded under one `Task`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |

The ACP `prompt` binary adds `prompt/4KiB` (100 turns per sample) and `prompt/1MiB`
(16 turns). It drives the preflight, the connection's own frame queue, the physical writer, and terminal
drain through the named `FakeAcpAgent`, with unchanged default limits and no override. Session
opening and request construction are outside the clock; final cleanup and the fake's wire capture
are included. It needs `--features testing`, which `scripts/bench.sh` supplies. This measures the
shared prompt path; admitting larger frames is a separate functional result, not a speedup.

Reading the output: one line per case with the median, minimum, maximum, coefficient of variation
(CV) and a per-unit cost, then a `samples_ms:` line with every raw sample in run order. The raw
samples are what a receipt quotes.

Three things to know when reading them:

- The byte counter behind `turn_buffer_bytes` is private. The `events/emit+drain/*` cases are the
  authoritative measurement of it, since a change to it moves them. The
  `events/serialize-count/*` cases are a stand-in and no longer the counter's shape: they are the
  `serde_json` counting writer the counter used before it added up sizes itself. That is still the
  exact reference its answer is tested against, and the path it falls back to for a value it
  cannot size with certainty, so the stand-in stays as the number to compare with.
- `stage/*` cases time one step and drop what it returns inside the timing, so a step that
  allocates is charged for freeing too. The `pipeline/*` cases run every step, so they will not
  equal the sum of the stages: a step's output is warm in cache for the next one.
- `copies/stdio-send/*` cases do not time the release of the message. `LinkSender::send` takes its
  message by value and frees it after the write, and a free costs whatever the allocator makes it
  cost: glibc hands a 1 MiB buffer straight back to the kernel, which was about 98% of the 1 MiB
  case while the whole call was on the clock. Each send is timed from its call to the moment the
  sink's write ends, and a sample is the sum over its messages, so it holds the newline, the one
  write and one clock read per message. The 1 MiB sample is 20 sends and a few microseconds long,
  so read it for a step change (a send that copied its message would add a 1 MiB copy to each one),
  not for a few percent.

## Cases that depend on the compiler

The clean-text scan behind `normalize::sanitize_field` is fast only while the compiler turns its
loop into vector instructions; the workspace forbids `unsafe`, so there are no intrinsics to fall
back on. Rust 1.99 (LLVM 23) stopped doing that for a byte test written as a `matches!` pattern, and
`normalize/sanitize_field-1KiB/ascii-clean` went from about 0.28 ms to about 1.8 ms on the reference
machine with no source change. The test is now comparisons joined by `|`, which has no branch to
lose.

The clean-prefix scan behind `normalize::bound_text` tests 16 bytes at a time and is written the
same way. Its `matches!` form was still vectorized on 1.97 to 1.99, but only because LLVM removed
the pattern's branches first.

The line-feed search behind `LineStream` tests 32 bytes at a time for the same reason.

The byte counter behind `turn_buffer_bytes` sums each string's escape cost over 32-byte blocks with
byte arithmetic (`stream::size::escaped_len`), so it is also fast only while the loop is vectorized;
a version over 16-byte blocks of the same expression fell to scalar code about 15 times slower in a
standalone probe. A vectorized counter holds `psadbw` on x86-64, as `objdump -d` on the `events`
bench shows. When the toolchain moves, compare `events/emit+drain/delta-1KiB-ascii` and
`events/emit+drain/delta-1KiB-escapes`.

The redactor behind `redact::stderr_text` finds the bytes it has to stop at the same way, 32 at a
time: a `:` or `=` for the credential rules, and the first byte of a removed character for the
control-character stripper. Text between two of them is copied without being read a word at a
time.

When `rust-toolchain.toml` moves, build the `events`, `framing` and `redact` binaries with the old
and the new toolchain (`RUSTUP_TOOLCHAIN` and a `CARGO_TARGET_DIR` each) and compare
`normalize/sanitize_field-1KiB/*`, `normalize/bound_text-4KiB/clean-ascii`,
`framing/records-1KiB/chunk-16KiB` and `redact/stderr_text-16KiB/plain`. A scalar loop is a
several-fold step in the clean cases and the 1 KiB records, with the `dirty` case and
`framing/records-15B/chunk-16KiB` unmoved, not a few percent. The redactor does more than search,
so there the step is smaller: a byte-wise search made the plain cases about 1.7 times slower.
`objdump -d` on the bench binary confirms it: a vectorized `sanitize_owned` holds `pcmpeqb`
instructions on x86-64, a scalar one holds none. A vectorized `bound_text` holds `pminub`, and a
vectorized `first_line_feed` or `redact::scan::first_match` holds `pcmpeqb`.

`first_match` has to be inlined into the locator that calls it (`#[inline]`): left as a call, the
search is still vectorized, but the call and the setup around each `:` cost about a quarter more
instructions on plain tails, and the bigger locators in `redact::rules` made the compiler stop
inlining it. A byte every rule turns away has to be turned away on that byte before a keyword is
compared, in `starts_a_keyword` and by `name_run`, which looks at the bytes of a name once.
Check `objdump -d` on `redact_bearer` and `redact_assignments` for `pcmpeqb`.

An instruction-count receipt is more stable than wall time for a change to the rules. On Linux
where `perf_event_open` works for the caller's own children, count user-space instructions of
`BENCH_SAMPLES=1 <bench binary> <case>` and of `BENCH_SAMPLES=11`; the difference over ten is the
instructions of one sample, with the setup cancelled.

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

## Receipt: the redactor's new reads

The redactor change that added the token pass, escaped-JSON reads, multi-line containers, counts
and the URL settle was measured with user-space instructions, which do not move with the load of a
shared machine the way wall time does. One sample is the call or calls the case names (320 KiB per
sample for the 1 KiB and 16 KiB cases, one call for 120 KB and 1 MiB); the figure is
`(I(11 samples) - I(1 sample)) / 10`, counted with `perf_event_open` around the bench binary, so
process start and setup cancel. Base is `e172b8a` built with the same `benches/redact.rs` as the
variant (its assertions loosened for the shapes Base cannot redact), Variant is the head of the
branch, both on the pinned toolchain, release profile.

| Case                               | Base (instructions) | Variant (instructions) |   Delta |
| ---------------------------------- | ------------------: | ---------------------: | ------: |
| `stderr_text-1KiB/plain`           |          10,944,352 |             10,587,951 |   -3.3% |
| `stderr_text-1KiB/secrets`         |          23,925,488 |             23,713,914 |   -0.9% |
| `stderr_text-1KiB/controls`        |          16,414,354 |             16,288,555 |   -0.8% |
| `stderr_text-1KiB/separators`      |          18,555,102 |             18,525,391 |   -0.2% |
| `stderr_text-1KiB/late-non-ascii`  |           8,330,531 |              8,144,507 |   -2.2% |
| `stderr_text-1KiB/json`            |          39,533,086 |             39,014,358 |   -1.3% |
| `stderr_text-1KiB/json-secrets`    |          21,122,459 |             20,652,462 |   -2.2% |
| `stderr_text-1KiB/escaped-json`    |          15,453,094 |             19,629,611 |  +27.0% |
| `stderr_text-1KiB/pretty`          |          13,261,763 |             16,139,215 |  +21.7% |
| `stderr_text-16KiB/plain`          |          10,391,808 |             10,026,233 |   -3.5% |
| `stderr_text-16KiB/secrets`        |          23,790,605 |             23,693,164 |   -0.4% |
| `stderr_text-16KiB/controls`       |          16,348,711 |             16,237,724 |   -0.7% |
| `stderr_text-16KiB/separators`     |          18,060,355 |             18,035,193 |   -0.1% |
| `stderr_text-16KiB/late-non-ascii` |           8,041,984 |              7,863,641 |   -2.2% |
| `stderr_text-16KiB/json`           |          39,030,094 |             38,501,385 |   -1.4% |
| `stderr_text-16KiB/json-secrets`   |          20,899,420 |             20,423,433 |   -2.3% |
| `stderr_text-16KiB/escaped-json`   |          15,097,944 |             19,201,412 |  +27.2% |
| `stderr_text-16KiB/pretty`         |          11,967,823 |             15,361,770 |  +28.4% |
| `stderr_text-16KiB/names`          |              76,850 |                 78,423 |   +2.0% |
| `stderr_text-16KiB/schemes`        |              76,694 |                 78,188 |   +1.9% |
| `stderr_text-16KiB/quoted-names`   |              77,717 |                 79,131 |   +1.8% |
| `stderr_text-16KiB/quotes`         |              76,730 |                 78,119 |   +1.8% |
| `stderr_text-16KiB/slashes`        |              77,343 |                 78,788 |   +1.9% |
| `stderr_text-16KiB/containers`     |           3,131,281 |              3,743,941 |  +19.6% |
| `stderr_text-16KiB/sequence`       |              79,175 |                 87,627 |  +10.7% |
| `stderr_text-120KB/names`          |             523,379 |                534,556 |   +2.1% |
| `stderr_text-120KB/schemes`        |             524,002 |                535,244 |   +2.1% |
| `stderr_text-120KB/quoted-names`   |             524,628 |                535,744 |   +2.1% |
| `stderr_text-120KB/quotes`         |             523,645 |                534,969 |   +2.2% |
| `stderr_text-120KB/slashes`        |             523,823 |                535,001 |   +2.1% |
| `stderr_text-120KB/containers`     |          22,896,221 |             26,170,627 |  +14.3% |
| `stderr_text-120KB/sequence`       |             526,070 |                544,255 |   +3.5% |
| `stderr_text-1MiB/names`           |           4,529,918 |              4,628,175 |   +2.2% |
| `stderr_text-1MiB/schemes`         |           4,527,904 |              4,626,075 |   +2.2% |
| `stderr_text-1MiB/quoted-names`    |           4,528,755 |              4,626,898 |   +2.2% |
| `stderr_text-1MiB/quotes`          |           4,527,637 |              4,625,984 |   +2.2% |
| `stderr_text-1MiB/slashes`         |           4,527,814 |              4,626,184 |   +2.2% |
| `stderr_text-1MiB/containers`      |         200,023,482 |            227,142,713 |  +13.6% |
| `stderr_text-1MiB/sequence`        |           4,529,864 |              4,635,389 |   +2.3% |
| `bounded-16KiB/bounded-plain`      |          10,457,034 |             22,506,760 | +115.2% |
| `bounded-16KiB/bounded-tokens`     |           5,467,762 |             17,563,729 | +221.2% |

Reading it:

- **`stderr_text` on the cases that existed** (`plain`, `secrets`, `controls`, `separators`,
  `late-non-ascii`, `json`, `json-secrets`): 0.1% to 3.5% fewer instructions than Base, from the
  quote-state rewrite in `redact::rules` that early-rejects with an inlined check. The change is
  allowed up to +3% on plain stderr; it is -3.5%.
- **The new shapes for `stderr_text`** cost what their extra reads cost. `escaped-json` and `pretty`
  do more because Base left them unread (+27% and +28%); the adversarial families (`names`,
  `schemes`, `quoted-names`, `quotes`, `slashes`, `sequence`) cost +2% over Base, a constant 0.1
  instruction per byte, and `containers` (an opener at the end of every line that never closes,
  190 instructions a byte in Base and 217 now) +14%. Instructions per byte are flat from 16 KiB to
  1 MiB in every family.
- **`bounded`** (`redact::bounded`, `redact::structured`) runs the token pass. Plain 16 KiB lines
  cost 2.2 times Base (10.5 M to 22.5 M for 320 KiB); lines with unnamed tokens 3.2 times, since
  Base leaves them unread. The four 1 MiB token-pass shapes cost 4.6 M to 55 M (a prefix before every
  run 41 M, web-token headers 34 M, one run opening like a token 55 M, a private-key header with no
  end 4.6 M; Base 4.5 M each). They are linear.
- **Rejected: the token pass inside `stderr_text`.** The same pass over plain stderr cost +120% (10.39 M
  to 22.5 M a sample at 16 KiB, 10.94 M to 23.0 M at 1 KiB). A gate on a byte class (`_`, `-`, `I`,
  `J`) is useless because ordinary tails contain `_` and `-` constantly; a first prototype saved
  only 6.8% of the cost. The pass therefore runs where the text is a vendor's message and not a
  tail, and the limit is written down in `docs/compliance.md`.
- **Wall time** on the shared reference machine is too noisy to carry the claim (the spread of
  per-run medians exceeds the delta for most plain cases); medians of five interleaved runs of 15
  samples each agree in direction with the instruction counts for the adversarial families and for
  `bounded`.

Adversarial timings, 1 MiB, one call per sample, release, median of five interleaved runs of 15
samples, ns per byte, with plain 16 KiB stderr beside them (range of the five medians in
parentheses):

| Family                                                     |              Base |           Variant |
| ---------------------------------------------------------- | ----------------: | ----------------: |
| `stderr_text` plain 16 KiB                                 | 4.2 (3.5 to 24.6) | 6.1 (3.2 to 31.0) |
| `names` (`token_` repeated, then `=`)                      |               3.7 |               2.4 |
| `schemes` (`a.` repeated, then `://`)                      |               1.1 |               0.9 |
| `quoted-names`                                             |               1.2 |               1.0 |
| `quotes`                                                   |               1.2 |               0.9 |
| `slashes` (escaped quotes three levels deep)               |               1.2 |               0.9 |
| `containers` (`token: [` and a line break, repeated)       |              41.5 |              31.6 |
| `sequence` (one YAML sequence as long as the text)         |               1.1 |               0.9 |
| `bounded` prefixes (`ghp_abc` repeated)                    |               0.9 |               5.2 |
| `bounded` private-key header with no end                   |               1.2 |               0.5 |
| `bounded` web-token headers with no claims                 |               1.1 |               3.7 |
| `bounded` one run as long as the text opening like a token |               0.9 |               5.5 |

The wall figures have a spread the size of the differences in the plain row; they show that no
family leaves the linear band (a quadratic family would be hundreds of times plain at 1 MiB), and
the instruction counts above carry the comparison. `redact/work.rs` asserts the same linearity on
every run by counting scan steps, with no clock.

The long searches that back the other claims (fixed point, no panic, agreement with 0.4.1) are
ignored tests in `redact/search.rs`: `MEA_REDACT_SEARCH_COUNT=4000000 cargo nextest run --release
--run-ignored only redact::search`, with `CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true` to trap an
overflow.

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
  noise is small next to it. When the measured call cannot be separated from work the case must
  not pay for, such as an API that frees its input before returning, use
  `bench.run_measured(name, unit, setup, routine)`: the routine times the part it is charged for
  and returns that duration beside its output.
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
