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
    assert!(
        watched.launcher.stdin_closes() <= 1,
        "expected transport closes: at most 1 | received: {}",
        watched.launcher.stdin_closes()
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
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
}

/// A close owns teardown first; the watcher then wakes on the ended pipe and must not reap again.
#[tokio::test]
async fn acp_teardown_close_then_watcher_kills_once() {
    let watched = open_watched(false).await;
    watched.launcher.release();

    tokio::time::timeout(
        Duration::from_secs(5),
        watched.session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close to return, not hang")
    .expect("expected close to succeed");
    watched.agent_gone.cancel();
    settle().await;

    assert_one_teardown(&watched);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
}

/// Dropping a pending close and then the session itself while cleanup is held leaves one owner,
/// which still kills the child once when the host control is released.
#[tokio::test]
async fn acp_teardown_concurrent_close_and_drop_kill_once() {
    let watched = open_watched(false).await;
    let closing = {
        let session = Arc::clone(&watched.session);
        tokio::spawn(async move { session.close(CloseReason::Shutdown).await })
    };
    tokio::time::timeout(Duration::from_secs(5), watched.launcher.wait_for_kill())
        .await
        .expect("expected close to reach the process kill");
    closing.abort();
    let _ = closing.await;
    let WatchedSession {
        session,
        launcher,
        inner,
        agent_gone,
    } = watched;
    drop(session);
    agent_gone.cancel();
    settle().await;
    launcher.release();

    tokio::time::timeout(Duration::from_secs(5), async {
        while inner.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the one owned cleanup to reap the child after its callers were dropped");
    settle().await;
    assert_eq!(
        launcher.kills(),
        1,
        "expected kills: 1 | received: {}",
        launcher.kills()
    );
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
    assert_ne!(
        status,
        SessionStatus::Closed,
        "expected session status: not Closed while the kill is held | received: {status:?}"
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
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
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
    assert_ne!(
        status,
        SessionStatus::Closed,
        "expected session status: not Closed after failed cleanup | received: {status:?}"
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
fn acp_teardown_fixture_child() {
    use std::io::{BufRead as _, Write as _};
    if std::env::var(FIXTURE_MODE).is_err() {
        return;
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // libtest has already printed `test <name> ... ` without a newline; end that line so the
    // first JSON-RPC reply starts on a line of its own.
    let _ = writeln!(stdout);
    let _ = stdout.flush();
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
                String::from("acp_teardown_fixture_child"),
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
