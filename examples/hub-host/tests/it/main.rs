//! The integration tests of `hub-host`, linked as one binary.
//!
//! One module per former test file: a target of its own costs a link of the whole dependency tree.

mod bench_runner;
mod common;
mod hub_fake;
mod recovery;
mod retry_policy;
mod start_failures;
mod stopping;
mod subscribers;
