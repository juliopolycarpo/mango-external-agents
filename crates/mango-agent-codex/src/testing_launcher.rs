use std::sync::Arc;

use mango_external_agents::process::{LaunchSpec, ManagedProcess, ProcessLauncher};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};

use super::MINIMUM_CODEX_VERSION;

/// Answers Codex version probes while delegating other launches unchanged.
///
/// Only the exact two-element argv `codex --version`, or the same probe for an explicitly
/// configured executable, is intercepted. The probe does not consume a queued process or enter
/// the inner launch log. Other programs, arguments, environment, cwd, I/O and controls belong to
/// the inner launcher.
pub struct VersionAnsweringLauncher {
    inner: Arc<dyn ProcessLauncher>,
    executable: Option<String>,
    version_line: String,
}

impl VersionAnsweringLauncher {
    /// Wraps `inner`, using this crate's minimum version in the `codex-cli` banner by default.
    #[must_use]
    pub fn new(inner: Arc<dyn ProcessLauncher>) -> Self {
        Self {
            inner,
            executable: None,
            version_line: format!("codex-cli {MINIMUM_CODEX_VERSION}"),
        }
    }

    /// Wraps `inner` with the default answer in a shareable launcher.
    #[must_use]
    pub fn over(inner: Arc<dyn ProcessLauncher>) -> Arc<Self> {
        Arc::new(Self::new(inner))
    }

    /// Also answers a probe whose program exactly matches this host-selected executable.
    ///
    /// No basename matching or filesystem lookup occurs. The plain `codex` program remains
    /// recognised; a path such as `/host/bin/codex` must be configured explicitly.
    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<String>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Sets the line supplied to the fake transcript, including older or unparseable banners.
    ///
    /// This lets a host exercise version refusal and unknown-version behavior. The line uses
    /// [`FakeProcess::transcript`]'s framing rather than running a real vendor executable.
    #[must_use]
    pub fn with_version_line(mut self, line: impl Into<String>) -> Self {
        self.version_line = line.into();
        self
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for VersionAnsweringLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        let codex = spec.program().is_some_and(|program| {
            program == "codex" || self.executable.as_deref() == Some(program)
        });
        if !codex || spec.argv.len() != 2 || spec.argv[1] != "--version" {
            return self.inner.spawn(spec).await;
        }
        let answer = FakeLauncher::new();
        answer.push(FakeProcess::transcript([self.version_line.clone()]));
        answer.spawn(spec).await
    }
}
