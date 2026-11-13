//! Comparing what an agent printed against the floor a profile pins.
//!
//! The comparison is the core's [`mango_external_agents::version`], re-exported under the path
//! this crate has always offered: dotted numbers of any length, because ACP agents version
//! themselves however they like and Cursor versions by date.

pub use mango_external_agents::version::{Comparison, compare, parse};
