//! One teardown owner per ACP session: the connection-loss watcher, `close` and drop all join the
//! same cleanup, so the child is killed once and every close waiter sees that cleanup's result.

use super::*;

/// Bounds long enough that a held kill is never mistaken for a timed-out one.
fn teardown_host(launcher: &GatedLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_secs(5),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

/// One session over a gated child whose stdout the test can end, to wake the watcher.
struct WatchedSession {
    session: Arc<dyn Session>,
    launcher: GatedLauncher,
    inner: FakeLauncher,
    agent_gone: CancelToken,
}

async fn open_watched(fail_kill: bool) -> WatchedSession {
    let inner = FakeLauncher::new();
    let agent_gone = CancelToken::new();
    inner.push(
        FakeAcpAgent::new()
            .process()
            .ending_stdout_when(agent_gone.clone()),
    );
    let launcher = GatedLauncher::new(inner.clone(), fail_kill);
    let opened = AcpHarness::new(profile())
        .open_session(&teardown_host(&launcher), OpenSession::new("teardown"))
        .await
        .expect("expected a session");
    WatchedSession {
        session: Arc::from(opened),
        launcher,
        inner,
        agent_gone,
    }
}

/// Lets any second teardown owner that was wrongly started reach the process control.
async fn settle() {
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
}

async fn wait_for_closed(session: &Arc<dyn Session>) {
    let mut lifecycle = session.subscribe();
    let status = status_once_settled(&mut lifecycle).await;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
}

fn assert_one_teardown(watched: &WatchedSession) {
    assert_eq!(
        watched.launcher.kills(),
        1,
        "expected kills: 1 | received: {}",
        watched.launcher.kills()
    );
    assert_eq!(
        watched.inner.live_children(),
        0,
        "expected live children: 0 | received: {}",
        watched.inner.live_children()
    );
}

/// The watcher tears down a dead agent first; a later close joins that result instead of killing
/// the child a second time.
#[tokio::test]
async fn acp_teardown_watcher_then_close_kills_once() {
    let watched = open_watched(false).await;
    watched.launcher.release();
    watched.agent_gone.cancel();
    wait_for_closed(&watched.session).await;

    tokio::time::timeout(
        Duration::from_secs(5),
        watched.session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close after the watcher to return, not hang")
    .expect("expected close to observe the watcher's successful cleanup");
    settle().await;

    assert_one_teardown(&watched);
    let status = watched.session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
}

/// A host clock that advances one microsecond on every read, so the order of two stamped facts
/// is the order in which the harness produced them.
struct TickingClock {
    ticks: std::sync::atomic::AtomicU64,
}

impl Clock for TickingClock {
    fn now(&self) -> SystemTime {
        let tick = self.ticks.fetch_add(1, Ordering::AcqRel);
        SystemTime::UNIX_EPOCH + Duration::from_micros(tick)
    }
}

/// `Closed` means nothing more will happen, so a watcher that finished the shared cleanup must
/// not publish it while the close that owns the turn still has its terminal to write.
///
/// The ticking clock stamps both facts: the snapshot that first reads `Closed` must be stamped
/// after the turn's terminal event.
#[tokio::test]
async fn acp_teardown_closed_is_published_after_the_turn_terminal() {
    assert_closed_after_terminal(true).await;
}

/// Without any close, the prompt task writes the turn's terminal after the watcher's cleanup;
/// the watcher waits for the turn to leave its slot before it publishes `Closed`.
#[tokio::test]
async fn acp_teardown_closed_without_close_is_published_after_the_turn_terminal() {
    assert_closed_after_terminal(false).await;
}

/// Drives a dead agent with a live turn to `Closed`, optionally with a close claimed while the
/// shared cleanup is held, and asserts the turn's terminal was stamped first.
async fn assert_closed_after_terminal(with_close: bool) {
    let inner = FakeLauncher::new();
    let agent_gone = CancelToken::new();
    inner.push(
        FakeAcpAgent::new()
            .never_finishing_turns()
            .process()
            .ending_stdout_when(agent_gone.clone()),
    );
    let launcher = GatedLauncher::new(inner.clone(), false);
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .clock(Arc::new(TickingClock {
            ticks: std::sync::atomic::AtomicU64::new(1),
        }))
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_secs(5),
            ..Limits::default()
        })
        .build()
        .expect("expected a host");
    let session: Arc<dyn Session> = Arc::from(
        AcpHarness::new(profile())
            .open_session(
                &host,
                OpenSession::new("terminal-first").with_configuration(permissive()),
            )
            .await
            .expect("expected a session"),
    );
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "keep working"))
        .await
        .expect("expected a turn");
    let mut lifecycle = session.subscribe();

    agent_gone.cancel();
    tokio::time::timeout(Duration::from_secs(5), launcher.wait_for_kill())
        .await
        .expect("expected the watcher to reach the process kill");
    // `close` claims the session synchronously on its first poll, so polling it once is the
    // observable that it owns the turn before the held kill is released.
    let mut closing = with_close.then(|| {
        let session = Arc::clone(&session);
        Box::pin(async move { session.close(CloseReason::Requested).await })
    });
    if let Some(closing) = closing.as_mut() {
        tokio::select! {
            biased;
            _ = closing.as_mut() => panic!("expected close to wait on the held cleanup"),
            () = std::future::ready(()) => {}
        }
    }
    launcher.release();

    let mut closed_at = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = lifecycle.current();
            if current.status == SessionStatus::Closed {
                closed_at = Some(current.observed_at);
                return;
            }
            let _ = lifecycle.changed().await;
        }
    })
    .await
    .expect("expected the session to reach Closed");
    if let Some(closing) = closing {
        tokio::time::timeout(Duration::from_secs(5), closing)
            .await
            .expect("expected close to settle after release")
            .expect("expected close to succeed");
    }

    let events = drain_events(&mut turn).await;
    let terminal_at = events
        .iter()
        .find(|event| event.is_terminal())
        .map(|event| event.at)
        .expect("expected the turn to receive a terminal");
    let closed_at = closed_at.expect("expected a Closed snapshot");
    assert!(
        closed_at > terminal_at,
        "expected Closed stamped after the turn terminal ({terminal_at:?}) | received: Closed at \
         {closed_at:?}"
    );
}

/// Reads every event a turn produced, bounded so a missing terminal fails instead of hanging.
async fn drain_events(turn: &mut TurnStream) -> Vec<mango_external_agents::AgentEvent> {
    let mut events = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(5), turn.recv()).await {
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

/// While the watcher's cleanup is stalled at the kill, every close waiter stays pending and the
/// session is not yet `Closed`; release lets all of them observe the same success.
#[tokio::test]
async fn acp_teardown_close_waits_for_a_stalled_watcher_cleanup() {
    let watched = open_watched(false).await;
    watched.agent_gone.cancel();
    tokio::time::timeout(Duration::from_secs(5), watched.launcher.wait_for_kill())
        .await
        .expect("expected the watcher to reach the process kill");

    let mut waiters = Vec::new();
    for reason in [CloseReason::Requested, CloseReason::Shutdown] {
        let session = Arc::clone(&watched.session);
        waiters.push(tokio::spawn(async move { session.close(reason).await }));
    }
    for waiter in &mut waiters {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), waiter)
                .await
                .is_err(),
            "expected close waiter: pending while cleanup is held | received: completed early"
        );
    }
    let status = watched.session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Closing,
        "expected session status: Closing while the kill is held | received: {status:?}"
    );

    watched.launcher.release();
    for waiter in waiters {
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("expected the close waiter to settle after release")
            .expect("expected the close task to join")
            .expect("expected every waiter to observe the shared successful cleanup");
    }
    settle().await;
    assert_one_teardown(&watched);
    let status = watched.session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
}

/// A failed watcher-owned cleanup is the result every close waiter observes; none reports success
/// and the session never claims `Closed`.
#[tokio::test]
async fn acp_teardown_a_failed_watcher_cleanup_fails_every_close_waiter() {
    let watched = open_watched(true).await;
    watched.agent_gone.cancel();
    tokio::time::timeout(Duration::from_secs(5), watched.launcher.wait_for_kill())
        .await
        .expect("expected the watcher to reach the process kill");

    let mut waiters = Vec::new();
    for reason in [CloseReason::Requested, CloseReason::Shutdown] {
        let session = Arc::clone(&watched.session);
        waiters.push(tokio::spawn(async move { session.close(reason).await }));
    }
    for waiter in &mut waiters {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), waiter)
                .await
                .is_err(),
            "expected close waiter: pending while cleanup is held | received: completed early"
        );
    }

    watched.launcher.release();
    for waiter in waiters {
        let result = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("expected the close waiter to settle after release")
            .expect("expected the close task to join");
        let error = result.expect_err("expected close result: Err(cleanup failed) | received: Ok");
        assert!(
            error.cleanup_control().is_some(),
            "expected a cleanup-required error carrying the process control | received: {error:?}"
        );
    }
    settle().await;
    assert_eq!(
        watched.launcher.kills(),
        1,
        "expected kills: 1 | received: {}",
        watched.launcher.kills()
    );
    let status = watched.session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Closing,
        "expected session status: Closing after failed cleanup | received: {status:?}"
    );
}

/// Selects the re-exec'd ACP fixture mode in the child copy of this test binary.
#[cfg(target_os = "linux")]
const FIXTURE_MODE: &str = "MEA_ACP_TEARDOWN_FIXTURE";

/// Not a test: the body a re-exec'd copy of this binary runs as a real ACP agent process.
///
/// It answers `initialize` and `session/new`, then ignores stdin EOF and sleeps, so only a kill
/// can end it. Without the environment marker it returns at once.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "re-exec'd fixture body; only runs as a real child process"]
fn acp_teardown_fixture_child() {
    use std::io::{BufRead as _, Write as _};
    if std::env::var(FIXTURE_MODE).is_err() {
        return;
    }
    let stdin = std::io::stdin();
    // Re-exec'd with `--quiet`, libtest's only preamble is a complete `running 1 test` line,
    // which the ACP reader skips as a non-frame.
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let result = match message.get("method").and_then(serde_json::Value::as_str) {
            Some("initialize") => serde_json::json!({
                "protocolVersion": 1,
                "agentInfo": { "name": "fixture-acp", "version": "1.0.0" },
                "agentCapabilities": { "loadSession": false },
                "authMethods": [],
            }),
            Some("session/new") => serde_json::json!({ "sessionId": "sess_fixture" }),
            _ => serde_json::json!({}),
        };
        let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = writeln!(stdout, "{reply}");
        let _ = stdout.flush();
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// A real child process spawned through the Tokio launcher is alive until `close`, is killed once
/// by the shared teardown owner, and is gone afterwards even when the session is dropped too.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn acp_teardown_real_process_is_killed_once_and_reaped() {
    let executable = std::env::current_exe().expect("expected the test binary's own path");
    let profile = Arc::new(
        AcpProfile::custom(
            "fixture",
            [
                String::from("fixture-acp"),
                String::from("--ignored"),
                String::from("--exact"),
                String::from("teardown::acp_teardown_fixture_child"),
                String::from("--quiet"),
                String::from("--nocapture"),
                String::from("--test-threads=1"),
            ],
            VENDOR,
        )
        .with_vendor_environment_keys(&[FIXTURE_MODE]),
    );
    let counting = CountingTokioLauncher::default();
    let host = HostContext::builder()
        .launcher(Arc::new(counting.clone()))
        .cwd(std::env::temp_dir())
        .environment(mango_external_agents::EnvSource::from_pairs([(
            FIXTURE_MODE,
            "1",
        )]))
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            kill_grace: Duration::from_millis(100),
            shutdown_timeout: Duration::from_secs(5),
            ..Limits::default()
        })
        .build()
        .expect("expected a host");
    let session: Arc<dyn Session> = Arc::from(
        tokio::time::timeout(
            Duration::from_secs(20),
            AcpHarness::new(profile)
                .with_executable(mango_external_agents::ExecutablePath::resolved(executable))
                .open_session(&host, OpenSession::new("real-teardown")),
        )
        .await
        .expect("expected the fixture agent to open a session in time")
        .expect("expected a session over the real fixture process"),
    );
    let pid = counting.pid().expect("expected the launched child's pid");
    let _cleanup = FixtureCleanup {
        pid: Some(pid),
        directory: None,
    };
    let proc_entry = std::path::PathBuf::from(format!("/proc/{pid}"));

    // A child that died on its own (for example on SIGPIPE) would make the teardown look clean.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        proc_entry.exists(),
        "expected fixture child {pid}: alive before teardown | received: exited"
    );

    let closing = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close(CloseReason::Requested).await })
    };
    let second = session.close(CloseReason::Shutdown);
    let (first, second) = tokio::join!(closing, second);
    first
        .expect("expected the close task to join")
        .expect("expected the first close to succeed");
    second.expect("expected the joined close to succeed");
    drop(session);
    settle().await;

    assert_eq!(
        counting.kills(),
        1,
        "expected kills: 1 | received: {}",
        counting.kills()
    );
    assert!(
        !proc_entry.exists(),
        "expected fixture child {pid}: reaped after close | received: still present"
    );
}

/// Kills a fixture child that outlived a failed assertion and removes its scratch directory.
///
/// Unwinding past a failed assert would otherwise leave a sleeping re-exec'd test binary behind.
#[cfg(target_os = "linux")]
struct FixtureCleanup {
    pid: Option<u32>,
    directory: Option<std::path::PathBuf>,
}

#[cfg(target_os = "linux")]
impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        if let Some(directory) = &self.directory {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

/// Wraps the real Tokio launcher and counts kills on the one child it spawns.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
struct CountingTokioLauncher {
    kills: Arc<AtomicUsize>,
    pid: Arc<Mutex<Option<u32>>>,
}

#[cfg(target_os = "linux")]
impl CountingTokioLauncher {
    fn kills(&self) -> usize {
        self.kills.load(Ordering::Acquire)
    }

    fn pid(&self) -> Option<u32> {
        *self
            .pid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl ProcessLauncher for CountingTokioLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<ManagedProcess> {
        let process = mango_external_agents::launcher::TokioLauncher::new()
            .spawn(spec)
            .await?;
        *self
            .pid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = process.control.pid();
        Ok(ManagedProcess {
            control: Arc::new(CountingProcessControl {
                inner: process.control,
                kills: Arc::clone(&self.kills),
            }),
            ..process
        })
    }
}

/// A session dropped on a thread with no Tokio runtime still reaps its child through the runtime
/// its connection runs on, instead of silently skipping cleanup.
#[tokio::test(flavor = "multi_thread")]
async fn acp_teardown_a_session_dropped_outside_a_runtime_reaps_its_child() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("drop-off-runtime"))
        .await
        .expect("expected a session");
    std::thread::spawn(move || drop(session))
        .join()
        .expect("expected the dropping thread to finish");

    let reaped = tokio::time::timeout(Duration::from_secs(5), async {
        while launcher.live_children() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        reaped.is_ok(),
        "expected live children: 0 after a drop outside a runtime | received: {}",
        launcher.live_children()
    );
}

/// Once the watcher has seen the agent go, the session admits no new turn: a start that slipped
/// in would run on a dead connection and could still be writing its terminal after `Closed`.
#[tokio::test]
async fn acp_teardown_no_turn_is_admitted_after_the_watcher_fires() {
    let watched = open_watched(false).await;
    watched.agent_gone.cancel();
    tokio::time::timeout(Duration::from_secs(5), watched.launcher.wait_for_kill())
        .await
        .expect("expected the watcher to reach the process kill");

    let started = watched
        .session
        .start_turn(TurnRequest::new("turn-after-exit", "too late"))
        .await;
    watched.launcher.release();
    let Err(error) = started else {
        panic!("expected start after agent exit: Err(Closed) | received: Ok(stream)");
    };
    assert!(
        matches!(error.cause(), Error::Closed { .. }),
        "expected start after agent exit: Err(Closed) | received: {error:?}"
    );
    wait_for_closed(&watched.session).await;
}
