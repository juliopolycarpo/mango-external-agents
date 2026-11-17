//! The integration tests of `mango-agent-acp`, linked as one binary.
//!
//! One module per former test file: a target of its own costs a link of the whole dependency tree.
//! The live-agent smoke test stays in `tests/smoke.rs`, because a profile's verification evidence
//! names that file.

mod discovery;
mod session;
mod version_floor;
