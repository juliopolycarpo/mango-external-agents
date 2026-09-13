#![doc = include_str!("../README.md")]

pub mod content;
pub mod error;
pub mod permission;
pub mod profile;
pub mod reducer;
pub mod transport;
pub mod version;

pub use profile::{AcpProfile, SessionModeIds, builtin_profile, builtin_profiles};

/// The harness kind this crate implements, as the core registry names it.
///
/// A whole [`HarnessKind`](mango_external_agents::HarnessKind) also names the profile — `acp:cursor`,
/// `acp:opencode` — because one ACP harness drives one agent.
pub const HARNESS_KIND: &str = "acp";
