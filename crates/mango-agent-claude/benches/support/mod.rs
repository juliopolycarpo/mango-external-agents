//! A dependency-free benchmark runner shared by this crate's bench binaries.
//!
//! There is no statistics engine here on purpose: each case runs a fixed number of samples, and
//! the runner prints every raw sample beside the median, minimum, maximum and coefficient of
//! variation, so a Base / Variant / Delta receipt can quote the numbers it computed from.
//!
//! The Codex, ACP and Claude crates carry byte-identical copies of this file at
//! `crates/mango-agent-codex/benches/support/mod.rs`,
//! `crates/mango-agent-acp/benches/support/mod.rs` and
//! `crates/mango-agent-claude/benches/support/mod.rs`; a crate cannot package a file that lives in
//! another crate. Change all four together.
//!
//! Usage from a bench binary:
//!
//! ```ignore
//! let bench = support::Bench::new("framing");
//! bench.run("line/1MiB", support::Unit::new(1 << 20, "byte"), || setup(), |input| work(input));
//! ```
//!
//! Environment: `BENCH_SAMPLES` sets the sample count (default 15). Any command-line argument
//! that does not start with `-` filters cases by substring, so `cargo bench -p <crate> --bench
//! framing -- chunk-4KiB` runs only the matching cases.

#![allow(dead_code)]

use std::hint::black_box;
use std::time::{Duration, Instant};

/// Samples per case when `BENCH_SAMPLES` is not set.
const DEFAULT_SAMPLES: usize = 15;

/// Unrecorded runs before the first sample, to warm caches and the allocator.
const WARMUP_RUNS: usize = 2;

/// How much work one routine call does, so a case can print a per-unit cost.
#[derive(Clone, Copy)]
pub struct Unit {
    count: u64,
    label: &'static str,
}

impl Unit {
    /// `count` units of `label` per call, for example `Unit::new(1000, "event")`.
    pub const fn new(count: u64, label: &'static str) -> Self {
        Self { count, label }
    }
}

/// Reads the `BENCH_SAMPLES` value: absent means the default, anything else must be a positive
/// integer, because a run with no samples has no median to report.
///
/// The error names the received value and the expected shape, for example
/// `parse_samples(Some("0"))`.
pub fn parse_samples(requested: Option<&str>) -> Result<usize, String> {
    let Some(raw) = requested else {
        return Ok(DEFAULT_SAMPLES);
    };
    match raw.parse::<usize>() {
        Ok(samples) if samples > 0 => Ok(samples),
        _ => Err(format!(
            "expected BENCH_SAMPLES to be a positive integer, received {requested:?}"
        )),
    }
}

/// One benchmark binary: its title, its sample count and its case filter.
pub struct Bench {
    samples: usize,
    filters: Vec<String>,
}

impl Bench {
    /// Reads the sample count and filters, then prints the environment header.
    pub fn new(title: &str) -> Self {
        let requested = std::env::var("BENCH_SAMPLES").ok();
        let samples =
            parse_samples(requested.as_deref()).unwrap_or_else(|message| panic!("{message}"));
        let filters = std::env::args()
            .skip(1)
            .filter(|argument| !argument.starts_with('-'))
            .collect();
        println!("# bench: {title}");
        println!(
            "# target: {}-{} | build: {} | samples: {samples} (+{WARMUP_RUNS} warmup)",
            std::env::consts::ARCH,
            std::env::consts::OS,
            if cfg!(debug_assertions) {
                "DEBUG (numbers are not comparable; use cargo bench)"
            } else {
                "optimized"
            },
        );
        Self { samples, filters }
    }

    /// Whether `name` passes the command-line filter.
    pub fn selected(&self, name: &str) -> bool {
        self.filters.is_empty() || self.filters.iter().any(|filter| name.contains(filter))
    }

    /// Times `routine` on a fresh `setup()` result, once per sample, and prints the summary.
    ///
    /// `setup` is not timed, and neither is dropping what `routine` returns. Put everything the
    /// measured code must not pay for, such as building input chunks, in `setup`.
    pub fn run<I, O>(
        &self,
        name: &str,
        unit: Unit,
        mut setup: impl FnMut() -> I,
        mut routine: impl FnMut(I) -> O,
    ) {
        if !self.selected(name) {
            return;
        }
        for _ in 0..WARMUP_RUNS {
            black_box(routine(setup()));
        }
        let mut samples = Vec::with_capacity(self.samples);
        for _ in 0..self.samples {
            let input = setup();
            let start = Instant::now();
            let output = routine(black_box(input));
            let elapsed = start.elapsed();
            drop(black_box(output));
            samples.push(elapsed);
        }
        print_summary(name, unit, &samples);
    }
}

/// A runtime for driving async code from a synchronous bench, one `block_on` per sample.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("expected a current-thread tokio runtime to build")
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn print_summary(name: &str, unit: Unit, samples: &[Duration]) {
    let mut sorted: Vec<f64> = samples.iter().copied().map(millis).collect();
    sorted.sort_by(f64::total_cmp);
    let count = sorted.len();
    let median = if count % 2 == 1 {
        sorted[count / 2]
    } else {
        f64::midpoint(sorted[count / 2 - 1], sorted[count / 2])
    };
    let mean = sorted.iter().sum::<f64>() / count as f64;
    let variance = sorted
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / count as f64;
    let cv = if mean > 0.0 {
        variance.sqrt() / mean * 100.0
    } else {
        0.0
    };
    let per_unit = median * 1_000_000.0 / unit.count.max(1) as f64;
    println!(
        "{name:<48} median {median:>10.4} ms | min {:>10.4} | max {:>10.4} | cv {cv:>5.1}% | {per_unit:>10.1} ns/{}",
        sorted[0],
        sorted[count - 1],
        unit.label,
    );
    let raw: Vec<String> = samples
        .iter()
        .copied()
        .map(|sample| format!("{:.4}", millis(sample)))
        .collect();
    println!("    samples_ms: {}", raw.join(" "));
}

// A bench target built with `harness = false` cannot run `#[test]`s, so these run where the file is
// included as a module of a test crate: `examples/hub-host/tests/bench_runner.rs`. The five copies
// are byte-identical, so that one run covers them all.
#[cfg(test)]
mod tests {

    #[test]
    fn an_absent_value_is_the_default_sample_count() {
        let received = super::parse_samples(None);

        assert_eq!(
            received,
            Ok(super::DEFAULT_SAMPLES),
            "expected the default sample count when BENCH_SAMPLES is unset | received {received:?}"
        );
    }

    #[test]
    fn a_positive_integer_is_the_sample_count() {
        let received = super::parse_samples(Some("25"));

        assert_eq!(
            received,
            Ok(25),
            "expected 25 samples for BENCH_SAMPLES=25 | received {received:?}"
        );
    }

    #[test]
    fn zero_and_non_numbers_are_refused_naming_the_value_and_the_expected_shape() {
        for raw in ["0", "abc", "-3", ""] {
            let received = super::parse_samples(Some(raw));
            let expected =
                format!("expected BENCH_SAMPLES to be a positive integer, received Some({raw:?})");

            assert_eq!(
                received,
                Err(expected.clone()),
                "expected BENCH_SAMPLES={raw:?} to be refused as `{expected}` | received {received:?}"
            );
        }
    }
}
