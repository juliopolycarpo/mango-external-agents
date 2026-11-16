//! The integration tests of `mango-agent-claude`, linked as one binary.
//!
//! One module per former test file: a target of its own costs a link of the whole dependency tree.
//! Everything except `debug_records` drives the harness against the fake CLI, which only exists
//! under `testing`, so the `--no-default-features` lane compiles just that one.

#[cfg(feature = "testing")]
mod cleanup_required;
#[cfg(feature = "testing")]
mod conformance;
#[cfg(feature = "testing")]
mod deadlines;
mod debug_records;
#[cfg(feature = "testing")]
mod fake_cli;
#[cfg(feature = "testing")]
mod harness_over_a_fake_cli;
#[cfg(feature = "testing")]
mod reducer_replay;
#[cfg(feature = "testing")]
mod resumability;
#[cfg(feature = "testing")]
mod support;
