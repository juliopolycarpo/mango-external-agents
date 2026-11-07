//! Named fake gateway responses, separate from archival vendor recordings.

use super::{replay_limits, workspace_path};
use mango_agent_codex::CodexHarness;
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{EnvSource, Harness, HostContext, OpenSession, SessionQuery};
use serde_json::{Value, json};
use std::sync::Arc;

struct GatewayServer {
    reply: Option<Value>,
}

impl GatewayServer {
    fn respond(&self, line: &str) -> Vec<String> {
        let frame: Value = serde_json::from_str(line).expect("client JSON");
        let result = match frame["method"].as_str() {
            Some("initialize") => json!({"result": {"userAgent": "codex-cli 0.166.0"}}),
            Some("initialized") => return Vec::new(),
            Some("account/gatewayOAuth/read") => match &self.reply {
                Some(reply) => reply.clone(),
                None => return Vec::new(),
            },
            Some("account/read") => json!({"result": {"requiresOpenaiAuth": false}}),
            Some("model/list" | "permissionProfile/list" | "thread/list") => {
                json!({"result": {"data": [], "nextCursor": null}})
            }
            _ => json!({"error": {"code": -32601, "message": "fake unsupported method"}}),
        };
        let mut answer = result;
        answer["id"] = frame["id"].clone();
        vec![answer.to_string()]
    }
}

fn gateway_host(reply: Option<Value>) -> (HostContext, Arc<FakeLauncher>) {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::transcript(["codex-cli 0.166.0"]));
    let server = GatewayServer { reply };
    launcher.push(FakeProcess::responding(move |line| server.respond(line)));
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(workspace_path())
        .client_info("gateway-test", "0.0.0")
        .environment(EnvSource::empty())
        .limits(replay_limits())
        .build()
        .expect("fake gateway host");
    (host, launcher)
}

fn written(launcher: &FakeLauncher) -> Vec<Value> {
    launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[tokio::test]
async fn gateway_readiness_is_explicit_and_precedes_models_on_each_connection() {
    for (required, status) in [(false, Value::Null), (true, json!("succeeded"))] {
        let (host, launcher) = gateway_host(Some(json!({"result": {
            "required": required, "status": status,
            "providerName": "private-provider-canary", "error": "private-error-canary"
        }})));
        let found = CodexHarness::new()
            .discover(&host)
            .await
            .expect("discovery");
        let frames = written(&launcher);
        let initialize = frames
            .iter()
            .find(|f| f["method"] == "initialize")
            .expect("initialize");
        assert_eq!(
            initialize["params"]["capabilities"]["explicitGatewayOauth"],
            true
        );
        let gateway = frames
            .iter()
            .position(|f| f["method"] == "account/gatewayOAuth/read")
            .expect("gateway read");
        let models = frames
            .iter()
            .position(|f| f["method"] == "model/list")
            .expect("models");
        assert!(gateway < models);
        assert!(!format!("{found:?}").contains("canary"));
        assert_eq!(launcher.live_children(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn gateway_unproved_or_unready_blocks_every_authenticated_entrypoint() {
    let replies = [
        None,
        Some(json!({"error": {"code": -32601, "message": "unsupported gateway"}})),
        Some(json!({"result": {}})),
        Some(json!({"result": {"required": "false"}})),
        Some(json!({"result": {"required": true, "status": null}})),
        Some(json!({"result": {"required": true, "status": "notReady"}})),
        Some(json!({"result": {"required": true, "status": "started"}})),
        Some(json!({"result": {"required": true, "status": "failed"}})),
        Some(json!({"result": {"required": true, "status": "futureStatus"}})),
    ];
    for reply in replies {
        for entrypoint in ["discover", "open", "picker"] {
            let (host, launcher) = gateway_host(reply.clone());
            let harness = CodexHarness::new();
            match entrypoint {
                "discover" => {
                    let _ = harness.discover(&host).await;
                }
                "open" => {
                    assert!(
                        harness
                            .open_session(&host, OpenSession::new("gateway-test"))
                            .await
                            .is_err()
                    );
                }
                _ => {
                    assert!(
                        harness
                            .list_sessions(&host, SessionQuery::default())
                            .await
                            .is_err()
                    );
                }
            }
            let frames = written(&launcher);
            assert!(
                frames.iter().all(|f| matches!(
                    f["method"].as_str(),
                    Some("initialize" | "initialized" | "account/gatewayOAuth/read")
                )),
                "authenticated request before proven gateway readiness: entrypoint={entrypoint}, reply={reply:?}, frames={frames:?}"
            );
            assert_eq!(
                launcher.live_children(),
                0,
                "failed handshake must reap child"
            );
        }
    }
}
