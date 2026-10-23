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

| Binary                                | Cases                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `mango-external-agents` / `framing`   | `LineStream`: a 128 KiB line at 16 KiB chunks; a 1 MiB line at 4 KiB, 16 KiB and 1 MiB chunks; 15-byte, 100-byte, 1 KiB and 64 KiB records at 16 KiB chunks; an invalid-UTF-8 line (the lossy repair path); malformed lines of 16 KiB, 64 KiB and 512 KiB with the first invalid byte at the start, middle and end, a multibyte character cut short, one invalid byte in 64 and all invalid, each handing its repaired line back from the routine so that line is freed outside the clock, and a dense-invalid line at the raw cap that repair pushes past the buffered limit (this one frees its repaired string and the stream's buffer inside the clock, as the bench cannot return them); the captured fixture transcripts replayed at 4 KiB and 16 KiB chunks                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `mango-external-agents` / `events`    | `EventSink::emit` plus drain for 1 KiB deltas (ASCII, JSON escapes, non-ASCII, characters the sanitiser strips), 16- and 64-byte deltas, approval requests and resolutions, a completed activity and a rich activity; the same events serialized to a counting writer and to a string; `normalize::sanitize_field` alone; `normalize::bound_text` at the detail limit over clean ASCII, a late or early non-ASCII character, an escape at, before and after the bound, and ANSI command output; the emit and drain of an activity whose detail has a late non-ASCII character or escape                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `mango-external-agents` / `redact`    | `redact::stderr_text` over a 1 KiB and a 16 KiB stderr tail, 320 KiB per sample: plain diagnostic lines, lines that carry each credential shape, coloured output with control characters throughout, and lines dense in separators and box-drawing characters with nothing to redact; plain lines that end in a non-ASCII character; structured log lines with a quote before nearly every `:`, with nothing to redact and with quoted credentials; JSON inside strings at two and three levels of escaping; a pretty-printed object with credentials in multi-line containers and a YAML sequence; `redact::bounded` over plain lines, lines with unnamed tokens, credentials, JSON inside strings and containers, and four 1 MiB token-pass shapes; a credential name made of one keyword repeated, a URL scheme made of dotted letters, quoted names that never get a value, a run of bare quotes, escaped quotes, an opener that never closes and one long sequence, each at 16 KiB, 120 KB and 1 MiB, through `stderr_text` and through `bounded`, one call per sample; `redact::stderr_tail` over 16 KiB tails of plain lines, credentials, tokens and headers (320 KiB per sample, beside `stderr_text` over the same text) and seven 1 MiB shapes: the four token-pass shapes, a cut key (lines of base64 from the first byte with no end marker), and the two name shapes; the begin-marker shape again at 4 MiB and 16 MiB |
| `mango-external-agents` / `copies`    | `Client` dispatch of prebuilt frames through a scripted link (1 KiB notification, 500 KB diff notification, 500 KB and error responses); `Client` sends of 1 KiB notifications into a discarding sink and 1 KiB requests against a peer that answers each as it is written, the same notifications from eight callers contending for one link, and both queued with `submit_*`; `stdio::open` link sends of 1 KiB and 1 MiB messages into a counting sink, each timed from its call to the end of its write                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `mango-agent-acp` / `reducer`         | `Reducer::update_at` on tool-call frames: ten 100 KiB diffs as a new call, as an update and as a completion; a 1 MiB image ahead of a text block; a 1 MiB text body; the frames most agents send (two 2 KiB diffs, a short text); and a 1000- and 10000-call `session/load` replay, through `update_at` and through `Reducer::session_facts`; held updates of one call (2 KiB to 1 MiB)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `mango-agent-codex` / `pipeline`      | Per stage and end to end: raw line to JSON, `Notification::parse`, `TurnReducer::reduce`, emit, drain, serialize. 1 KiB text deltas; 500 KB patch updates (throttled and emitted); a file change started and completed; command output inside the throttle window (1 KiB, 8 KiB and 64 KiB chunks); every captured Codex transcript                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `hub-host` / `retry`                  | `Supervisor::run` re-sending an 8 MiB, 2 MiB, 1 MiB or attachment-free request after `NotSubmitted` answers (1 or 3 re-sends), on a paused clock so backoff costs nothing; the process's peak resident set is printed after the cases                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `mango-agent-claude` / `tool_results` | `TurnReducer::reduce` closing a call: a `tool_result` record with a string payload (1 MiB, 32 KiB) or an array of text blocks (1 MiB in one block and in 100 blocks, 12 KiB and 32 KiB in one block, 6 KiB and 600 bytes in three); starting a `Write` (16 KiB, 200 KiB) or an `Edit` (two 2 KiB or two 100-byte strings) call; 1000 subagent text blocks of 1 KiB forwarded under one `Task`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |

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
Check `objdump -d` on `bearer_stage` and `assignment_stage` (in `redact::floor`) for `pcmpeqb`.

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

## Receipt: the redactor's newer reads

In this historical receipt, "release before" means Base `e172b8a`: the 0.6.1 rules with the fixes
merged after that release. The measurements below keep their original comparison points.

The redactor change that added the token pass, escaped-JSON reads, multi-line containers, sequences,
block scalars, counts and the URL settle was measured with user-space instructions, which do not move
with the load of a shared machine the way wall time does. One sample is the call or calls the case
names (320 KiB per sample for the 1 KiB and 16 KiB cases, one call for 120 KB and 1 MiB); the figure
is `(I(11 samples) - I(1 sample)) / 10`, counted with `perf_event_open` around the bench binary, so
process start and setup cancel. Base is `e172b8a` built with the same `benches/redact.rs` as the
variant (its assertions loosened for the shapes Base cannot redact), Variant is the head of the
branch, both on the pinned toolchain in the release profile **with
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1`**.

That last setting matters. With the default 16 code generation units, the instruction count of an
unchanged path moves by up to 3% when an unrelated edit changes how the crate is split into units
and so which functions can be inlined into which. One unit has no such partition, and every commit
that does not touch the path measures the same, to within 150 instructions in 22 million.

**`stderr_text` is the release before's code.** It runs the rules in `redact::floor`, which are
Base's `rules.rs` to the letter, so a tail costs what it did: plain lines within 0.3% and every
other case within 3.0%. The two ways a copy of those rules can cost more are measured and
removed: a second scan of the same text for the spans the union needs (the three stages are one
function each, compiled once, and both the text and the spans come from it), and a second set of
callers for the scan helpers, which made the compiler stop inlining them into the rules (plain
lines +5.9%). The +3.0% on
`secrets` is the keyword check that decides whether a redacted URL password needs the assignment
rule once more (about 540 instructions for each URL, on the one line in nine of this case that
carries one).

| Case                               | Base (instructions) | Variant (instructions) | Delta |
| ---------------------------------- | ------------------: | ---------------------: | ----: |
| `stderr_text-1KiB/plain`           |          10,606,270 |             10,618,017 | +0.1% |
| `stderr_text-1KiB/secrets`         |          22,597,131 |             23,250,358 | +2.9% |
| `stderr_text-1KiB/controls`        |          16,502,368 |             16,031,633 | -2.9% |
| `stderr_text-1KiB/separators`      |          18,621,647 |             18,628,452 | +0.0% |
| `stderr_text-1KiB/late-non-ascii`  |           8,164,097 |              8,184,740 | +0.3% |
| `stderr_text-1KiB/json`            |          38,738,727 |             38,729,427 | -0.0% |
| `stderr_text-1KiB/json-secrets`    |          20,455,996 |             20,457,628 | +0.0% |
| `stderr_text-1KiB/escaped-json`    |          15,366,941 |             15,365,326 | -0.0% |
| `stderr_text-1KiB/pretty`          |          13,008,325 |             13,017,727 | +0.1% |
| `stderr_text-16KiB/plain`          |          10,054,717 |             10,038,121 | -0.2% |
| `stderr_text-16KiB/secrets`        |          22,503,149 |             23,185,685 | +3.0% |
| `stderr_text-16KiB/controls`       |          16,462,735 |             15,961,528 | -3.0% |
| `stderr_text-16KiB/separators`     |          18,149,010 |             18,123,100 | -0.1% |
| `stderr_text-16KiB/late-non-ascii` |           7,899,935 |              7,890,576 | -0.1% |
| `stderr_text-16KiB/json`           |          38,247,800 |             38,181,145 | -0.2% |
| `stderr_text-16KiB/json-secrets`   |          20,240,070 |             20,212,077 | -0.1% |
| `stderr_text-16KiB/escaped-json`   |          15,036,231 |             15,004,619 | -0.2% |
| `stderr_text-16KiB/pretty`         |          11,751,233 |             11,732,681 | -0.2% |
| `stderr_text-16KiB/names`          |              78,287 |                 78,286 | -0.0% |
| `stderr_text-16KiB/schemes`        |              78,058 |                 78,074 | +0.0% |
| `stderr_text-16KiB/quoted-names`   |              79,200 |                 79,258 | +0.1% |
| `stderr_text-16KiB/quotes`         |              78,250 |                 78,375 | +0.2% |
| `stderr_text-16KiB/slashes`        |              78,806 |                 78,167 | -0.8% |
| `stderr_text-16KiB/containers`     |           2,946,487 |              2,944,104 | -0.1% |
| `stderr_text-16KiB/sequence`       |              80,622 |                 79,948 | -0.8% |
| `stderr_text-120KB/names`          |             534,411 |                533,670 | -0.1% |
| `stderr_text-120KB/schemes`        |             535,184 |                534,740 | -0.1% |
| `stderr_text-120KB/quoted-names`   |             535,801 |                535,169 | -0.1% |
| `stderr_text-120KB/quotes`         |             534,843 |                535,174 | +0.1% |
| `stderr_text-120KB/slashes`        |             534,884 |                535,478 | +0.1% |
| `stderr_text-120KB/containers`     |          21,543,295 |             21,530,431 | -0.1% |
| `stderr_text-120KB/sequence`       |             537,144 |                537,612 | +0.1% |
| `stderr_text-1MiB/names`           |           4,628,022 |              4,628,636 | +0.0% |
| `stderr_text-1MiB/schemes`         |           4,626,104 |              4,626,743 | +0.0% |
| `stderr_text-1MiB/quoted-names`    |           4,626,883 |              4,627,413 | +0.0% |
| `stderr_text-1MiB/quotes`          |           4,625,854 |              4,626,483 | +0.0% |
| `stderr_text-1MiB/slashes`         |           4,626,046 |              4,626,456 | +0.0% |
| `stderr_text-1MiB/containers`      |         188,201,496 |            188,085,496 | -0.1% |
| `stderr_text-1MiB/sequence`        |           4,628,180 |              4,628,515 | +0.0% |

**`bounded`** (`redact::bounded`, `redact::structured`, and the extension text, the peer label and
`mea`'s probe failures) reads the rules of the release before, the newer reads and the token pass,
over the same text, joins the spans, and reads the result again when it changed. Base has no
token pass and none of the newer reads, so its row is the cost of `stderr_text`. Plain 16 KiB
lines cost 3.2 times Base (10.1 M to 32.1 M for 320 KiB), of which the rules of the release before
are 10.0 M, the newer reads 8.3 M and the token pass 13.2 M; a text that holds credentials is read
twice, so a credential-dense text costs several times a tail. The cost is for a vendor's message,
short and rare, and not for a tail. The four 1 MiB token-pass shapes are linear.

| Case                                 | Base (instructions) | Variant (instructions) |   Delta |
| ------------------------------------ | ------------------: | ---------------------: | ------: |
| `bounded-16KiB/bounded-plain`        |          10,119,734 |             32,078,072 | +217.0% |
| `bounded-16KiB/bounded-tokens`       |           5,357,419 |             44,838,916 | +736.9% |
| `bounded-16KiB/bounded-secrets`      |                   - |            165,308,214 |         |
| `bounded-16KiB/bounded-escaped-json` |                   - |             88,848,855 |         |
| `bounded-16KiB/bounded-pretty`       |                   - |             77,803,356 |         |
| `bounded-16KiB/names`                |                   - |                534,259 |         |
| `bounded-16KiB/schemes`              |                   - |              1,225,392 |         |
| `bounded-16KiB/quoted-names`         |                   - |                733,285 |         |
| `bounded-16KiB/quotes`               |                   - |                675,985 |         |
| `bounded-16KiB/slashes`              |                   - |                712,840 |         |
| `bounded-16KiB/containers`           |                   - |             23,547,668 |         |
| `bounded-16KiB/sequence`             |                   - |              2,210,265 |         |
| `bounded-120KB/names`                |                   - |              3,829,753 |         |
| `bounded-120KB/schemes`              |                   - |              8,891,184 |         |
| `bounded-120KB/quoted-names`         |                   - |              5,283,661 |         |
| `bounded-120KB/quotes`               |                   - |              4,871,987 |         |
| `bounded-120KB/slashes`              |                   - |              5,134,265 |         |
| `bounded-120KB/containers`           |                   - |            171,130,067 |         |
| `bounded-120KB/sequence`             |                   - |             16,099,005 |         |
| `bounded-1MiB/names`                 |                   - |             33,376,472 |         |
| `bounded-1MiB/schemes`               |                   - |             77,590,850 |         |
| `bounded-1MiB/quoted-names`          |                   - |             46,059,452 |         |
| `bounded-1MiB/quotes`                |                   - |             42,463,115 |         |
| `bounded-1MiB/slashes`               |                   - |             44,751,379 |         |
| `bounded-1MiB/containers`            |                   - |          1,497,525,490 |         |
| `bounded-1MiB/sequence`              |                   - |            140,555,644 |         |
| `bounded-1MiB/prefixes`              |                   - |             48,515,533 |         |
| `bounded-1MiB/pem`                   |                   - |            118,562,639 |         |
| `bounded-1MiB/web`                   |                   - |             41,073,524 |         |
| `bounded-1MiB/one-run`               |                   - |             61,336,362 |         |

**`stderr_tail`** (`redact::stderr_tail`, which `StderrTail::read` and the stderr in a harness's
failure use) reads the rules of the release before and the token pass, joins their spans, and reads
the result again when the first reading changed it. It is not run per line: every production caller
(`StderrTail::read`, and the Claude, Codex and ACP harnesses when they put a child's stderr in an
error) reads a bounded tail once, when a failure is being reported. Base here is `stderr_text` over
the same text (the same binary), Variant is `stderr_tail`; both at one codegen unit. The `secrets`
and `headers` tails cost several times the rules alone because every line that holds a credential
changes the text, so the second reading runs. The 1 MiB shapes have no `stderr_text` row to be
compared with; they are linear (`redact/work.rs` counts the steps of `stderr_tail` over every shape
of the rules, of the token pass and of the cut key).

| Case                            | Base: `stderr_text` (instructions) | Variant: `stderr_tail` (instructions) |   Delta |
| ------------------------------- | ---------------------------------: | ------------------------------------: | ------: |
| `stderr_tail-16KiB/plain`       |                         10,038,319 |                            24,893,557 | +148.0% |
| `stderr_tail-16KiB/secrets`     |                         23,186,122 |                           109,434,652 | +372.0% |
| `stderr_tail-16KiB/tokens`      |                          5,306,486 |                            37,390,107 | +604.6% |
| `stderr_tail-16KiB/headers`     |                         25,266,245 |                            97,662,410 | +286.5% |
| `stderr_tail-1MiB/prefixes`     |                                  - |                            47,355,704 |         |
| `stderr_tail-1MiB/pem`          |                                  - |                           117,001,718 |         |
| `stderr_tail-1MiB/web`          |                                  - |                            39,913,501 |         |
| `stderr_tail-1MiB/one-run`      |                                  - |                            60,170,438 |         |
| `stderr_tail-1MiB/cut-key`      |                                  - |                            34,142,138 |         |
| `stderr_tail-1MiB/names`        |                                  - |                            32,209,392 |         |
| `stderr_tail-1MiB/quoted-names` |                                  - |                            44,892,262 |         |
| `stderr_tail-4MiB/pem`          |                                  - |                           467,551,378 |         |
| `stderr_tail-16MiB/pem`         |                                  - |                         1,887,858,358 |         |

Every row of this bench that `main` has was counted again on the head of the branch against `main` at `5fb3302`: 69 rows, the `stderr_text`, `bounded` and 1 MiB token-pass rows, both built at one codegen unit. None moved: the 53 rows of a million instructions or more are within 0.04%, and the 16 KiB rows of about 79,000 instructions, the smallest, are within 0.5% (all lower on the head, a difference of layout and not of work). The twelve `bounded-16KiB` rows are within 0.05%, the four that last under 0.1 ms (`names`, `quoted-names`, `quotes`, `slashes`) included, so a wall-clock ratio of 1.2 on one of them in a single run is the machine and not the code: the same `main` binary measured 0.052 and 0.082 ms for `quotes` in two of five alternating runs. `redact::message_text` is what `bounded` runs before it bounds the text, and `structured` runs the same union per string leaf; a one-off count of `bounded` over 16 KiB and 1 MiB of plain, credential and token lines and of `structured` over a nested value with credential leaves gave the same instructions on `main` and on the head to within 0.02%. The worst shape of the key read, begin markers with no end marker, is linear: 117,001,718 instructions at 1 MiB, 467,551,378 at 4 MiB (4.00 times) and 1,887,858,358 at 16 MiB (16.14 times).

`StderrTail::push` also reads what the tail drops, to know whether the dropped text ended inside a
private key (`redact::OpenKey`: four flags, a byte counter and a line buffer holding at most 2 x 4,168
bytes, no allocation on the usual path, and the lock is held for it as it is for the drain). A drop
is read in blocks of 4 KiB. A block with no hyphen is skipped by one byte search
(`<[u8]>::contains`, the standard library's memchr), and one with a hyphen is gated by a single
vectorised pass that looks for five hyphens after a capital or a space (the dashes a marker closes
with), for a hyphen first in the block, and for text the stripper acts on (a control character, a
C1 control, U+061C, a bidi mark or an isolate: `strip::may_strip`, two vectorised passes, the second
only when a byte can start one). Only a block that passes the gate is stripped and searched for
markers, so non-ASCII text without escapes is searched as it is, by the same byte search. A one-off
timing of pushing 64 MiB of log lines into a 16 KiB tail, `main` against the head, three runs each,
medians, in ns per byte (`main` / head), by the size of each push:

| Log                                                        |           1 B |          64 B |       4 KiB |      16 KiB |      64 KiB |
| ---------------------------------------------------------- | ------------: | ------------: | ----------: | ----------: | ----------: |
| `no-hyphen`, no hyphen in the text                         | 36.93 / 35.89 | 16.98 / 17.19 | 1.57 / 1.72 | 0.51 / 0.59 | 0.20 / 0.30 |
| `plain`, an ISO date (two hyphens) per line                | 35.79 / 38.07 | 16.27 / 18.71 | 1.64 / 2.01 | 0.51 / 0.92 | 0.22 / 0.54 |
| `cjk`, an ISO date and CJK text per line                   | 35.32 / 35.78 | 17.13 / 18.46 | 1.81 / 2.00 | 0.60 / 1.05 | 0.22 / 0.66 |
| `check-and-dash`, an ISO date, a check mark and an em dash | 37.14 / 39.98 | 18.28 / 20.62 | 1.98 / 2.48 | 0.59 / 1.37 | 0.23 / 1.11 |
| `with-keys`, a 24-line key every 200 lines                 | 37.65 / 36.38 | 16.94 / 18.47 | 1.69 / 2.29 | 0.50 / 0.96 | 0.21 / 0.70 |
| `hyphens`, a 40-dash line and `-----` per line             | 38.05 / 39.41 | 16.30 / 18.74 | 1.56 / 2.62 | 0.50 / 1.12 | 0.21 / 0.78 |
| `candidates`, `-----BEGIN-----` lines                      | 37.12 / 40.59 | 16.39 / 18.82 | 1.72 / 2.66 | 0.50 / 1.49 | 0.21 / 1.22 |

A push the size of a line or smaller costs the same within the noise of the runs. A push of 16 KiB
or more drops nearly all of what it carries, and that drop is what is read: a log with no hyphen
costs 0.10 ns per byte more at 64 KiB, a log with dates 0.32, the same log in CJK text 0.44 or with
a check mark and an em dash 0.88 (before the gate looked for what the stripper acts on, any hyphen
and any byte of 0x80 or more in a block sent it through the stripper, 5.1 ns per byte at 16 KiB),
and the worst shapes (every line carrying runs of dashes, or marker candidates) 0.57 and 1.01. At
64 KiB, 0.3 to 1.2 ns per byte is 20 to 80 microseconds for a push of that size, and the child's
pipe delivers far less than that per second.

Milliseconds for one 256 KiB text of each shape, release, default profile, the rules of the release
before (`stderr_text` on `main`), `stderr_text` now, `bounded` on `main` and now, and `structured` on
a string leaf now (`MEA_PROBE_IN=<file> MEA_PROBE_OUT=<file> cargo nextest run -p
mango-external-agents --release --lib --run-ignored only -E 'test(redact::search::probe_timing)'`):

| Text                 | `stderr_text` main | `stderr_text` now | `bounded` main | `bounded` now | `structured` now |
| -------------------- | -----------------: | ----------------: | -------------: | ------------: | ---------------: |
| `plain`              |                1.5 |               1.0 |            1.0 |           3.0 |              3.0 |
| `cred-lines`         |                3.0 |               3.0 |            3.0 |          20.2 |             19.7 |
| `seq-rep`            |                3.9 |               3.4 |            3.9 |          29.1 |             29.0 |
| `block-scalar-rep`   |                3.8 |               3.1 |            3.8 |          24.9 |             25.3 |
| `container-unclosed` |                5.0 |               7.1 |            5.5 |          57.6 |             59.2 |
| `container-nested`   |                3.6 |               5.1 |            3.7 |          40.0 |             35.9 |
| `name-chain`         |                4.6 |               5.4 |            4.8 |          41.8 |             43.1 |
| `url-pass-q`         |                4.4 |               5.9 |            4.6 |          44.5 |             45.3 |
| `esc-depth2`         |                0.7 |               1.3 |            0.8 |          13.8 |             14.4 |
| `bare-tokens`        |                0.2 |               0.2 |            0.2 |           2.2 |              2.6 |

The ratio of the message path to Base is the number of readers, and holds from 16 KiB to 1 MiB:
`redact/work.rs` asserts the linearity of every entry point on every run by counting scan steps
(and a JSON text inside a string, whose parse is not counted, was timed at 32 KiB to 250 KiB:
the time doubles when the text does).

- **Rejected: the token pass inside `stderr_text`.** The same pass over plain stderr cost +120% (10.39 M
  to 22.5 M a sample at 16 KiB, 10.94 M to 23.0 M at 1 KiB). A gate on a byte class (`_`, `-`, `I`,
  `J`) is useless because ordinary tails contain `_` and `-` constantly; a first prototype saved
  only 6.8% of the cost. The pass therefore runs where the text is a vendor's message, and for a
  stderr tail only in `redact::stderr_tail`, which reads a bounded tail once when a failure is
  reported and not line after line; `stderr_text` keeps the budget above, and the limit is written
  down in `docs/compliance.md`.
- **Rejected: the newer reads inside `stderr_text`.** They cost plain tails 2.4% and the lines with
  credentials 8.2% in a design that put them in the one rewrite, and made `stderr_text` hide less
  than the release before in places no search had reached (three rounds of review found one after
  another). They are read beside the rules of the release before, in the paths that carry a
  message, and `stderr_text` keeps the release before's answer.

The long searches that back the other claims (nothing the release before hid is shown, fixed point,
no panic, a JSON text stays JSON) are ignored tests in `redact/search.rs`:
`MEA_REDACT_SEARCH_COUNT=4000000 cargo nextest run --release --run-ignored only redact::search`,
with `CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true` to trap an overflow.

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
