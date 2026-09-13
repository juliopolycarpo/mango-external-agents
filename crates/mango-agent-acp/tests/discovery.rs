//! ACP host configuration boundaries.
#![cfg(feature = "testing")]

#[path = "discovery/policy.rs"]
mod policy;

use std::sync::Arc;

use mango_agent_acp::testing::FakeAcpAgent;
use mango_agent_acp::AcpHarness;
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{CloseReason, Error, HostContext, ProcessLauncher};

fn host(launcher: Arc<dyn ProcessLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .client_info("discovery-tests", "0.1.0")
        .build()
        .expect("expected a host")
}
