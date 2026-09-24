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

async fn drive_fake_agent(process: FakeProcess) -> (Arc<ConnectionHandle>, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(process);
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
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
    let cleanup = DriveShutdownGuard::from_launched(&launched, *host.limits());
    let connection = Arc::new(
        drive(launched, state, String::from("bounded-acp-tests"), cleanup)
            .await
            .expect("driver"),
    );
    (connection, launcher)
}

#[tokio::test(start_paused = true)]
async fn unsupported_session_messages_are_consumed_without_sdk_retry_storage() {
    let agent = UnknownMessageAgent::default();
    let (connection, _) = drive_fake_agent(agent.process()).await;
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

struct StalledDriver;

impl StalledDriver {
    fn start() -> tokio::task::JoinHandle<agent_client_protocol::Result<()>> {
        tokio::spawn(std::future::pending())
    }
}

#[tokio::test(start_paused = true)]
async fn aborting_and_joining_a_stalled_driver_still_reports_successful_reaping() {
    let (connection, launcher) = drive_fake_agent(UnknownMessageAgent::default().process()).await;
    // Replace the owned driver with one that cannot finish voluntarily. This injects the failure
    // at the driver boundary: transport writer gates alone do not stop the SDK from winding down.
    let previous = connection
        .driver
        .lock()
        .expect("driver")
        .take()
        .expect("owned driver");
    previous.abort();
    previous.await.expect_err("old driver was cancelled");
    let stalled = StalledDriver::start();
    let finished = stalled.abort_handle();
    *connection.driver.lock().expect("driver") = Some(stalled);
    connection.begin_shutdown(CancelReason::Shutdown);
    let result = connection.wait_shutdown().await;
    assert!(
        finished.is_finished(),
        "expected the driver to be joined before shutdown completes"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the child to be reaped"
    );
    result.expect(
        "expected successful cleanup result after both driver join and child reap were proven",
    );
}

/// Answers every request with a batch larger than the default 1,024-message transport budget
/// instead of a response, so the request is in flight when the overflow kills the connection.
#[derive(Clone, Default)]
struct FloodingAgent {
    received: mango_external_agents::CancelToken,
}

impl FloodingAgent {
    const BATCH: usize = 1_100;

    fn process(&self) -> FakeProcess {
        let agent = self.clone();
        FakeProcess::responding(move |_line| {
            agent.received.cancel();
            let batch: Vec<serde_json::Value> = (0..Self::BATCH)
                .map(|_| {
                    serde_json::json!({
                        "jsonrpc": "2.0", "method": "session/unsupported",
                        "params": {"sessionId": "native-1"}
                    })
                })
                .collect();
            vec![serde_json::Value::Array(batch).to_string()]
        })
    }
}

#[tokio::test]
async fn a_request_in_flight_when_an_incoming_budget_overflows_returns_limit_exceeded() {
    let agent = FloodingAgent::default();
    let (connection, launcher) = drive_fake_agent(agent.process()).await;
    let profile = crate::profile::builtin_profile("opencode").expect("opencode profile");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        send(
            &connection,
            &profile,
            Duration::from_secs(5),
            "initialize",
            agent_client_protocol::schema::v1::InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::V1,
            ),
        ),
    )
    .await
    .expect("expected the overflow to end the request before the test deadline");
    assert!(
        agent.received.is_cancelled(),
        "expected the request to reach the agent before the overflow"
    );
    let error = result.expect_err("expected the overflow to fail the in-flight request");
    let text = error.to_string();
    assert!(
        matches!(
            error,
            Error::LimitExceeded {
                limit: 1024,
                received: FloodingAgent::BATCH,
                ..
            }
        ),
        "expected LimitExceeded {{ limit: 1024, received: {} }}, received {text:?}",
        FloodingAgent::BATCH
    );
    connection.begin_shutdown(CancelReason::Shutdown);
    connection.wait_shutdown().await.expect("cleanup");
    assert_eq!(
        launcher.live_children(),
        0,
        "expected no live child after cleanup"
    );
}
