//! Test launchers for hosts that inject fake Codex processes.
//!
//! Enable this crate's `testing` feature in a development dependency. It forwards the core's
//! `testing` feature. Version answers are synthetic; other launches go to the injected launcher.
//!
//! ```
//! use std::sync::Arc;
//! use mango_agent_codex::testing::VersionAnsweringLauncher;
//! use mango_external_agents::testing::FakeLauncher;
//!
//! let inner = Arc::new(FakeLauncher::new());
//! let launcher = VersionAnsweringLauncher::new(inner)
//!     .with_executable("/host/bin/codex")
//!     .with_version_line("codex-cli 0.1.0");
//! let _shared = Arc::new(launcher);
//! ```

use crate::MINIMUM_CODEX_VERSION;

#[path = "testing_launcher.rs"]
mod launcher;

pub use launcher::VersionAnsweringLauncher;

#[cfg(test)]
#[path = "testing_tests.rs"]
mod tests;
