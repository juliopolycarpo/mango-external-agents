use super::*;
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    AcpSpec, HarnessIdentity, SessionIds, SessionSnapshot, StdioSpec, TransportKind,
    TransportSelection,
};

#[derive(Clone, Default)]
struct UnknownMessageAgent {
    answered: mango_external_agents::CancelToken,
    replies: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl UnknownMessageAgent {
    fn process(&self) -> FakeProcess {
        let agent = self.clone();
        let mut greeting = (0..128)
            .map(|_| {
                serde_json::json!({
                    "jsonrpc": "2.0", "method": "session/unsupported",
                    "params": {"sessionId": "native-1"}
                })
                .to_string()
            })
            .collect::<Vec<_>>();
        greeting.push(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 999, "method": "session/unsupported",
                "params": {"sessionId": "native-1"}
            })
            .to_string(),
        );
        FakeProcess::responding(move |line| agent.receive(line)).with_greeting(greeting)
    }

    fn receive(&self, line: &str) -> Vec<String> {
        let frame: serde_json::Value = serde_json::from_str(line).expect("SDK JSON frame");
        self.replies.lock().expect("replies").push(frame);
        self.answered.cancel();
        Vec::new()
    }
}

#[tokio::test(start_paused = true)]
async fn unsupported_session_messages_are_consumed_without_sdk_retry_storage() {
    let agent = UnknownMessageAgent::default();
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher))
        .cwd(std::env::temp_dir())
        .client_info("bounded-acp-tests", "0.1.0")
        .build()
        .expect("host");
    let core = mango_external_agents::SessionState::new(
        Arc::clone(host.clock()),
        SessionSnapshot::opening(
            SessionIds {
                session_id: SessionId::new("session-1"),
                native_session_id: String::from("native-1"),
            },
            HarnessIdentity::claude(),
            TransportSelection::new(None, TransportKind::Acp),
            host.now(),
        ),
    );
    let state = Arc::new(SessionState::new(
        SessionId::new("session-1"),
        &host,
        Configuration::default(),
        core,
    ));
    let launched = crate::transport::connect(
        &host,
        &AcpSpec::ChildPipes(StdioSpec::new(["fake-acp"])),
        &[],
    )
    .await
    .expect("transport");
    let connection = Arc::new(
        drive(launched, state, String::from("bounded-acp-tests"))
            .await
            .expect("driver"),
    );
    tokio::time::timeout(Duration::from_secs(1), agent.answered.cancelled())
        .await
        .expect("expected method-not-found response instead of retaining unknown session messages");
    let replies = agent.replies.lock().expect("replies").clone();
    assert_eq!(
        replies.len(),
        1,
        "unknown notifications must not receive responses"
    );
    assert_eq!(replies[0]["id"], 999);
    assert_eq!(replies[0]["error"]["code"], -32601);
    connection.begin_shutdown(CancelReason::Shutdown);
    connection.wait_shutdown().await.expect("cleanup");
}
