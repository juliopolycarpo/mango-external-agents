//! One teardown owner per Codex session: the shutdown watcher, `close` and drop all join the same
//! cleanup, so the JSON-RPC client closes once, the child is killed once, and every close waiter
//! sees that cleanup's result.

use super::*;

/// Bounds long enough that a held kill is never mistaken for a timed-out one on the real clock.
fn teardown_limits() -> mango_external_agents::Limits {
    mango_external_agents::Limits {
        kill_grace: std::time::Duration::from_millis(10),
        shutdown_timeout: std::time::Duration::from_secs(5),
        ..replay_limits()
    }
}

/// One session over a gated app-server whose host lifetime token the test controls.
struct WatchedSession {
    session: Arc<dyn Session>,
    launcher: Arc<GatedLauncher>,
    inner: Arc<FakeLauncher>,
    kill_gate: Arc<FakeGate>,
    cancel: mango_external_agents::CancelToken,
}

async fn open_watched(fail_kill: bool) -> WatchedSession {
    let inner = Arc::new(FakeLauncher::new());
    inner.push(Transcript::load("turn").as_process());
    let kill_gate = FakeGate::closed();
    let gated = GatedLauncher::new(Arc::clone(&inner), Some(Arc::clone(&kill_gate)), None);
    let launcher = Arc::new(if fail_kill {
        gated.failing_kills()
    } else {
        gated
    });
    let cancel = mango_external_agents::CancelToken::new();
    let host = host_context(
        Arc::clone(&launcher) as Arc<dyn ProcessLauncher>,
        None,
        teardown_limits(),
        cancel.clone(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("teardown"))
            .await
            .expect("expected a session"),
    );
    WatchedSession {
        session,
        launcher,
        inner,
        kill_gate,
        cancel,
    }
}

/// Opens the kill gate for more callers than one owner needs, so a duplicate reaper is counted
/// instead of hanging the test.
fn release_kills(gate: &FakeGate) {
    for _ in 0..8 {
        gate.open();
    }
}

/// Lets any second teardown owner that was wrongly started reach the process control.
async fn settle() {
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

fn assert_one_teardown(launcher: &GatedLauncher, inner: &FakeLauncher) {
    assert_eq!(
        launcher.kills(),
        1,
        "expected kills: 1 | received: {}",
        launcher.kills()
    );
    // `Client::close` is idempotent internally, so the transport close is the only observable of
    // how many owners reached it.
    assert_eq!(
        launcher.stdin_closes(),
        1,
        "expected client transport closes: 1 | received: {}",
        launcher.stdin_closes()
    );
    assert_eq!(
        inner.live_children(),
        0,
        "expected live children: 0 | received: {}",
        inner.live_children()
    );
}

/// The watcher tears down on host shutdown first; a later close joins that result instead of
/// closing the client or killing the child a second time.
#[tokio::test]
async fn codex_teardown_watcher_then_close_kills_once() {
    let watched = open_watched(false).await;
    release_kills(&watched.kill_gate);
    watched.cancel.cancel();
    wait_for_closed(&watched.session).await;

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close after the watcher to return, not hang")
    .expect("expected close to observe the watcher's successful cleanup");
    settle().await;

    assert_one_teardown(&watched.launcher, &watched.inner);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
}

/// A close owns teardown first; host shutdown afterwards must not start a second reaper.
#[tokio::test]
async fn codex_teardown_close_then_watcher_kills_once() {
    let watched = open_watched(false).await;
    release_kills(&watched.kill_gate);

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close to return, not hang")
    .expect("expected close to succeed");
    watched.cancel.cancel();
    settle().await;

    assert_one_teardown(&watched.launcher, &watched.inner);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
}

/// Dropping a pending close and then the session itself while cleanup is held leaves one owner,
/// which still kills the child once when the host control is released.
#[tokio::test]
async fn codex_teardown_concurrent_close_and_drop_kill_once() {
    let watched = open_watched(false).await;
    let closing = {
        let session = Arc::clone(&watched.session);
        tokio::spawn(async move { session.close(CloseReason::Shutdown).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.kill_gate.wait_until_entered(),
    )
    .await
    .expect("expected close to reach the process kill");
    closing.abort();
    let _ = closing.await;
    let WatchedSession {
        session,
        launcher,
        inner,
        kill_gate,
        cancel,
    } = watched;
    drop(session);
    cancel.cancel();
    settle().await;
    release_kills(&kill_gate);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while inner.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the one owned cleanup to reap the child after its callers were dropped");
    settle().await;
    assert_one_teardown(&launcher, &inner);
}

/// While the watcher's cleanup is stalled at the kill, every close waiter stays pending and the
/// session is not yet `Closed`; release lets all of them observe the same success.
#[tokio::test]
async fn codex_teardown_close_waits_for_a_stalled_watcher_cleanup() {
    let watched = open_watched(false).await;
    watched.cancel.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.kill_gate.wait_until_entered(),
    )
    .await
    .expect("expected the watcher to reach the process kill");

    let mut waiters = Vec::new();
    for reason in [CloseReason::Requested, CloseReason::Shutdown] {
        let session = Arc::clone(&watched.session);
        waiters.push(tokio::spawn(async move { session.close(reason).await }));
    }
    for waiter in &mut waiters {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
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

    release_kills(&watched.kill_gate);
    for waiter in waiters {
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("expected the close waiter to settle after release")
            .expect("expected the close task to join")
            .expect("expected every waiter to observe the shared successful cleanup");
    }
    settle().await;
    assert_one_teardown(&watched.launcher, &watched.inner);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
}

/// A failed watcher-owned cleanup is the result every close waiter observes; none reports success
/// and the session never claims `Closed`.
#[tokio::test]
async fn codex_teardown_a_failed_watcher_cleanup_fails_every_close_waiter() {
    let watched = open_watched(true).await;
    watched.cancel.cancel();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.kill_gate.wait_until_entered(),
    )
    .await
    .expect("expected the watcher to reach the process kill");

    let mut waiters = Vec::new();
    for reason in [CloseReason::Requested, CloseReason::Shutdown] {
        let session = Arc::clone(&watched.session);
        waiters.push(tokio::spawn(async move { session.close(reason).await }));
    }
    for waiter in &mut waiters {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
                .await
                .is_err(),
            "expected close waiter: pending while cleanup is held | received: completed early"
        );
    }

    release_kills(&watched.kill_gate);
    for waiter in waiters {
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
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

/// Selects the re-exec'd app-server fixture mode in the child copy of this test binary.
#[cfg(target_os = "linux")]
const FIXTURE_MODE: &str = "MEA_CODEX_TEARDOWN_FIXTURE";

/// Not a test: the body a re-exec'd copy of this binary runs as a real `codex app-server`.
///
/// It replays the recorded `turn` conversation over its own stdio and then outlives stdin EOF, so
/// only a kill can end it. Without the environment marker it returns at once.
#[cfg(target_os = "linux")]
#[test]
fn codex_teardown_fixture_child() {
    if std::env::var(FIXTURE_MODE).is_err() {
        return;
    }
    Transcript::load("turn").serve_stdio_until_killed();
}

/// Writes an executable `codex` stand-in that re-execs this test binary in fixture mode.
///
/// The Codex argv is fixed (`codex app-server ...`) and the environment allowlist drops unknown
/// keys, so the wrapper is where the fixture filter and marker are chosen.
#[cfg(target_os = "linux")]
fn write_fixture_executable() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let test_binary = std::env::current_exe().expect("expected the test binary's own path");
    let directory = std::env::temp_dir().join(format!(
        "mea-codex-teardown-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&directory).expect("expected a fixture directory");
    let wrapper = directory.join("codex");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{FIXTURE_MODE}=1 exec '{}' codex_teardown_fixture_child --nocapture --test-threads=1\n",
            test_binary.display()
        ),
    )
    .expect("expected the fixture wrapper to be written");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("expected the fixture wrapper to be executable");
    wrapper
}

/// A real app-server child spawned through the Tokio launcher is alive until `close`, is killed
/// once by the shared teardown owner, and is gone afterwards even when the session is dropped too.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn codex_teardown_real_process_is_killed_once_and_reaped() {
    let wrapper = write_fixture_executable();
    let counting = CountingTokioLauncher::default();
    let host = host_context(
        Arc::new(counting.clone()),
        None,
        mango_external_agents::Limits {
            kill_grace: std::time::Duration::from_millis(100),
            shutdown_timeout: std::time::Duration::from_secs(5),
            ..replay_limits()
        },
        mango_external_agents::CancelToken::new(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            CodexHarness::new()
                .with_executable(mango_external_agents::ExecutablePath::resolved(&wrapper))
                .open_session(&host, OpenSession::new("real-teardown")),
        )
        .await
        .expect("expected the fixture app-server to open a session in time")
        .expect("expected a session over the real fixture process"),
    );
    let pid = counting.pid().expect("expected the launched child's pid");
    let proc_entry = std::path::PathBuf::from(format!("/proc/{pid}"));

    // A child that died on its own (for example on SIGPIPE) would make the teardown look clean.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
    let _ = std::fs::remove_dir_all(wrapper.parent().unwrap_or(&wrapper));

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
    pid: Arc<std::sync::Mutex<Option<u32>>>,
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
    async fn spawn(&self, mut spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        // The replay's `/workspace` is a fixture placeholder, not a directory on this machine.
        spec.cwd = std::env::temp_dir();
        let process = mango_external_agents::launcher::TokioLauncher::new()
            .spawn(spec)
            .await?;
        *self
            .pid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = process.control.pid();
        Ok(ManagedProcess {
            control: Arc::new(CountingControl {
                inner: process.control,
                kills: Arc::clone(&self.kills),
            }),
            ..process
        })
    }
}

/// Counts kill requests on a real child while delegating every operation to it.
#[cfg(target_os = "linux")]
struct CountingControl {
    inner: Arc<dyn ProcessControl>,
    kills: Arc<AtomicUsize>,
}

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl ProcessControl for CountingControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> mango_external_agents::Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn interrupt(
        &self,
        reason: CancelReason,
    ) -> mango_external_agents::Result<InterruptOutcome> {
        self.inner.interrupt(reason).await
    }

    async fn kill(&self, reason: CancelReason) -> mango_external_agents::Result<()> {
        self.kills.fetch_add(1, Ordering::AcqRel);
        self.inner.kill(reason).await
    }
}
