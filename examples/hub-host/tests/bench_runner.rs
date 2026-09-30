//! Runs the shared benchmark runner's own tests.
//!
//! The runner lives in `benches/support/mod.rs`, and a bench target built with `harness = false`
//! cannot run `#[test]`s. Including the file as a module of this test crate runs the tests written
//! beside `parse_samples` under `cargo nextest` like any other. The runner is copied byte for byte
//! into every crate that has benches (`scripts/check-bench-runner.sh` keeps them identical), so
//! testing this copy tests them all.

#[path = "../benches/support/mod.rs"]
mod support;
