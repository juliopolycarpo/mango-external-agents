use super::*;

/// A named broker that deliberates until it is dropped, and counts how it is used.
///
/// `live` is the number of `decide` futures that have started and not yet been dropped, which is
/// what a host's own policy code (a network call, a person's prompt) is holding open on its side.
#[derive(Default)]
struct StuckCountingBroker {
    calls: AtomicUsize,
    live: AtomicUsize,
    peak: AtomicUsize,
}

/// Decrements `live` when the `decide` future is dropped, whichever way it ends.
struct LiveCall<'a>(&'a AtomicUsize);

impl Drop for LiveCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl mango_external_agents::PermissionBroker for StuckCountingBroker {
    async fn decide(&self, _request: &mango_external_agents::PermissionRequest) -> BrokerDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        let _call = LiveCall(&self.live);
        std::future::pending::<()>().await;
        BrokerDecision::Ask
    }
}

impl StuckCountingBroker {
    /// Waits for `calls` to reach `wanted`, naming the last count seen when it does not.
    async fn calls_reach(&self, wanted: usize) {
        let mut seen = self.calls.load(Ordering::SeqCst);
        let reached = tokio::time::timeout(Duration::from_secs(5), async {
            while seen < wanted {
                tokio::time::sleep(Duration::from_millis(5)).await;
                seen = self.calls.load(Ordering::SeqCst);
            }
        })
        .await;
        assert!(
            reached.is_ok(),
            "expected the broker to be asked {wanted} times | received {seen} calls"
        );
    }

    /// Waits for every started `decide` future to be dropped, naming the count left when it is not.
    async fn settles_to_no_live_calls(&self, when: &str) {
        let mut live = self.live.load(Ordering::SeqCst);
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            while live != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                live = self.live.load(Ordering::SeqCst);
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "expected no live broker calls {when} | received live={live} peak={} calls={}",
            self.peak.load(Ordering::SeqCst),
            self.calls.load(Ordering::SeqCst)
        );
    }
}

async fn open_with_broker(
    broker: &Arc<StuckCountingBroker>,
    max_pending_requests: usize,
) -> Box<dyn Session> {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let host = HostContext::builder()
        .launcher(Arc::new(launcher))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .broker(Arc::clone(broker) as Arc<dyn mango_external_agents::PermissionBroker>)
        .limits(Limits {
            max_pending_requests,
            ..Limits::default()
        })
        .build()
        .expect("host");
    AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat").with_configuration(permissive()),
        )
        .await
        .expect("expected session")
}

/// A broker call belongs to its question. Cancelling the turn withdraws the question, and the call
/// deliberating on it must end with it rather than run until the approval deadline. Otherwise a
/// host that cancels turns faster than its broker answers holds more callbacks than
/// `max_pending_requests` allows, one more per turn.
#[tokio::test]
async fn cancelling_a_turn_ends_the_broker_call_deliberating_on_its_question() {
    let broker = Arc::new(StuckCountingBroker::default());
    let session = open_with_broker(&broker, 1).await;
    for turn_number in 0..4 {
        let mut turn = session
            .start_turn(TurnRequest::new(format!("turn-{turn_number}"), "run it"))
            .await
            .unwrap_or_else(|error| {
                panic!("expected turn {turn_number} to be admitted | received {error:?}")
            });
        broker.calls_reach(turn_number + 1).await;
        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        drain(&mut turn).await;
        broker
            .settles_to_no_live_calls("after the turn was cancelled")
            .await;
    }
    let (peak, calls) = (
        broker.peak.load(Ordering::SeqCst),
        broker.calls.load(Ordering::SeqCst),
    );
    assert!(
        peak <= 1 && calls == 4,
        "expected at most max_pending_requests=1 broker calls live at once across 4 asks | received peak={peak} calls={calls}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

/// `close` ends the turn without a cancel. The call must not outlive the session that asked it.
#[tokio::test]
async fn closing_the_session_ends_the_broker_call_deliberating_on_its_question() {
    let broker = Arc::new(StuckCountingBroker::default());
    let session = open_with_broker(&broker, 8).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn", "run it"))
        .await
        .expect("expected turn");
    broker.calls_reach(1).await;
    session.close(CloseReason::Shutdown).await.expect("close");
    drain(&mut turn).await;
    broker
        .settles_to_no_live_calls("after the session was closed")
        .await;
}
