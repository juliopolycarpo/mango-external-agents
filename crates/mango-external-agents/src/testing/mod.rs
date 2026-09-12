//! Fakes a host or a harness is proven against.
//!
//! Behind the `testing` feature for dependants; always available to this crate's own tests.
//!
//! A test here spawns nothing. It scripts what a vendor would have said and asserts on what the
//! library made of it, which is the only way to test a dialect on a machine where that vendor's
//! CLI is not installed — every machine, in CI.

mod link;

pub use link::ScriptedLink;
