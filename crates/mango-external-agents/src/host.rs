//! What the host provides, and what the library is allowed to assume.
//!
//! [`HostContext`] is the whole of it: a launcher, a directory the host already authorised, the
//! environment it is willing to pass on, an optional scratch directory for artifacts that must be
//! visible to a child, who the host says it is, a clock, a cancellation token and the caps it
//! wants the library to read vendors under.
//!
//! There is no credential field here, and there never will be: the library reuses whatever the
//! user already logged into with the vendor's own CLI, reports that state without reading it, and
//! offers no way to smuggle a host secret into a child through the environment.
//!
//! That is a guarantee about this type, not a claim that no credential exists anywhere. A host
//! driving an endpoint that requires one passes it per dial on
//! [`WsSpec::with_bearer`](crate::WsSpec::with_bearer), where it is sent as one header on one
//! handshake and kept out of debug output and errors; and a harness that documents a vendor's own
//! API-key variable in `vendor_environment_keys` lets that one through by name. Both are
//! deliberate, both are narrow, and neither goes through the shared context.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::Notify;

use crate::env::EnvSource;
use crate::error::{Error, Result};
use crate::permission::PermissionBroker;
use crate::process::{DEFAULT_STDERR_TAIL_BYTES, LineLimits, ProcessLauncher};

/// Who the host says it is, for vendors that ask.
///
/// Codex's `clientInfo` and ACP's `initialize` both carry it. It is the host's own name, never the
/// library's: a vendor reading its logs should see which product launched it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// The host product's name.
    pub name: String,
    /// The host product's version.
    pub version: String,
}

impl std::fmt::Debug for ClientInfo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientInfo")
            .field("name_bytes", &self.name.len())
            .field("version_bytes", &self.version.len())
            .finish()
    }
}

impl ClientInfo {
    /// Names the host.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::ClientInfo;
    ///
    /// let client = ClientInfo::new("mangostudio", "1.4.0");
    /// assert_eq!(client.name, "mangostudio");
    /// ```
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// Where the library reads "now" from.
///
/// Injected so a test can stamp events without waiting for a real clock, and so a host that keeps
/// its own time source keeps one answer. Timers are a different thing and belong to the async
/// runtime; this is only the wall clock an event is stamped with.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> SystemTime;
}

/// The operating system's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A shutdown signal the host owns and the library observes.
///
/// Cloning shares one signal: every clone sees the cancellation, and cancelling any of them
/// cancels all. Owned by this crate rather than taken from a utility crate so a host is not made
/// to adopt one.
///
/// # Example
///
/// ```
/// use mango_external_agents::CancelToken;
///
/// let token = CancelToken::new();
/// let watcher = token.clone();
/// assert!(!watcher.is_cancelled());
/// token.cancel();
/// assert!(watcher.is_cancelled());
/// ```
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    state: Arc<CancelState>,
}

#[derive(Debug, Default)]
struct CancelState {
    cancelled: AtomicBool,
    changed: Notify,
}

impl CancelToken {
    /// A token nobody has cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels it, waking everything waiting. Idempotent.
    pub fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.state.changed.notify_waiters();
    }

    /// Whether it has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Resolves once it is cancelled, immediately if it already was.
    pub async fn cancelled(&self) {
        loop {
            // Registered before the check: a `cancel` landing between the two still wakes this.
            let changed = self.state.changed.notified();
            if self.is_cancelled() {
                return;
            }
            changed.await;
        }
    }
}

/// The caps the library reads a vendor under.
///
/// Every one of them exists because the vendor is a third-party process whose output is not the
/// host's to trust: a stalled reader must trigger bounded pressure handling, a long line must
/// be refused, and a stalled call must end rather than hold a turn open forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How many payload events one turn holds before reporting overflow and stopping native work.
    /// Interaction events have a separate count reserve within the shared byte budget.
    pub turn_channel_capacity: usize,
    /// Maximum serialized payload bytes queued per turn, excluding its reserved terminal.
    pub turn_buffer_bytes: usize,
    /// Maximum in-flight protocol requests or approval callbacks per session.
    pub max_pending_requests: usize,
    /// Line and buffer caps for framed transports.
    pub line: LineLimits,
    /// How much stderr is kept for diagnostics.
    pub stderr_tail_bytes: usize,
    /// How long one request waits for its answer before it is a failure.
    pub request_timeout: Duration,
    /// Maximum time a turn may remain silent without an outstanding host interaction.
    pub idle_timeout: Duration,
    /// Maximum time allowed for a session shutdown stage.
    pub shutdown_timeout: Duration,
    /// How long a vendor approval stays answerable before the harness refuses it.
    ///
    /// Separate from [`Self::request_timeout`]: a request deadline bounds a protocol call, while an
    /// approval is work deliberately waiting for a person or the host's policy. Keeping them apart
    /// lets a host fail a stalled handshake promptly without cutting an approval short.
    pub approval_timeout: Duration,
    /// How long a child is given to exit on its own before the launcher escalates.
    pub kill_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turn_channel_capacity: 1_024,
            turn_buffer_bytes: 8 * 1024 * 1024,
            max_pending_requests: 64,
            line: LineLimits::default(),
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
            request_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(120),
            shutdown_timeout: Duration::from_secs(5),
            approval_timeout: Duration::from_secs(30 * 60),
            kill_grace: Duration::from_secs(2),
        }
    }
}

impl Limits {
    /// Computes the wall-clock deadline for one approval.
    ///
    /// The configured timeout is checked instead of added unchecked. A host clock near the edge of
    /// SystemTime receives a typed configuration refusal instead of causing a harness callback to
    /// panic. For example, the default timeout after the Unix epoch produces a later instant.
    ///
    /// # Errors
    ///
    /// Returns Error::HostConfiguration when this timeout cannot be represented after now.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::SystemTime;
    ///
    /// use mango_external_agents::Limits;
    ///
    /// let expires_at = Limits::default()
    ///     .approval_expires_at(SystemTime::UNIX_EPOCH)
    ///     .expect("default approval deadline");
    /// assert!(expires_at > SystemTime::UNIX_EPOCH);
    /// ```
    pub fn approval_expires_at(&self, now: SystemTime) -> Result<SystemTime> {
        now.checked_add(self.approval_timeout)
            .ok_or_else(|| Error::HostConfiguration {
                expected: "an approval deadline representable from the host clock",
                received: format!(
                    "host clock {now:?} with approval timeout {:?}",
                    self.approval_timeout
                ),
            })
    }
}

/// Everything a harness may use without reaching into host state.
///
/// Cheap to clone: the launcher and the clock are behind `Arc`, and a harness that opens several
/// sessions shares one context.
#[derive(Clone)]
pub struct HostContext {
    launcher: Arc<dyn ProcessLauncher>,
    cwd: PathBuf,
    scratch: Option<PathBuf>,
    environment: EnvSource,
    client_info: ClientInfo,
    clock: Arc<dyn Clock>,
    cancel: CancelToken,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: Limits,
}

impl std::fmt::Debug for HostContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostContext")
            .field("scratch_configured", &self.scratch.is_some())
            .field("client_info", &self.client_info)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl HostContext {
    /// Starts building one.
    pub fn builder() -> HostContextBuilder {
        HostContextBuilder::default()
    }

    /// The host's process launcher.
    pub fn launcher(&self) -> &Arc<dyn ProcessLauncher> {
        &self.launcher
    }

    /// The directory the host authorised. The library never widens it.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The host-owned directory available for scoped artifacts a child must read.
    ///
    /// The host creates and authorises this directory, including any sandbox or container mapping
    /// that makes it visible to the child. A harness that needs it refuses its operation when this
    /// is `None`; it never falls back to a process-global temporary directory.
    pub fn scratch(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }

    /// The environment the host is willing to pass on, before the allowlist.
    pub fn environment(&self) -> &EnvSource {
        &self.environment
    }

    /// Who the host says it is.
    pub fn client_info(&self) -> &ClientInfo {
        &self.client_info
    }

    /// The host's cancellation signal.
    pub fn cancel(&self) -> &CancelToken {
        &self.cancel
    }

    /// The host's approval policy, when it has one.
    ///
    /// `None` means every approval reaches the host as an event, which is the default: nothing in
    /// the library grants a permission on its own.
    pub fn broker(&self) -> Option<&Arc<dyn PermissionBroker>> {
        self.broker.as_ref()
    }

    /// The caps the library reads vendors under.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The current instant, from the host's clock.
    pub fn now(&self) -> SystemTime {
        self.clock.now()
    }

    /// The host's clock, for a harness that stamps its own values.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The environment one child of this harness receives.
    ///
    /// The positive allowlist, built from the host's source and the harness's own documented keys.
    pub fn child_environment(
        &self,
        vendor_keys: &[&str],
    ) -> std::collections::BTreeMap<String, String> {
        crate::env::allowlist(&self.environment, vendor_keys)
    }
}

/// Builds a [`HostContext`], refusing one that could not launch anything.
#[derive(Default)]
pub struct HostContextBuilder {
    launcher: Option<Arc<dyn ProcessLauncher>>,
    cwd: Option<PathBuf>,
    scratch: Option<PathBuf>,
    environment: Option<EnvSource>,
    client_info: Option<ClientInfo>,
    clock: Option<Arc<dyn Clock>>,
    cancel: Option<CancelToken>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: Option<Limits>,
}

impl HostContextBuilder {
    /// The host's process launcher. Required.
    #[must_use]
    pub fn launcher(mut self, launcher: Arc<dyn ProcessLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// The directory the host already authorised. Required.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// A host-owned directory for scoped artifacts a child must read.
    ///
    /// The directory remains optional because most harness operations only need the authorised
    /// working directory. A harness that needs scratch storage refuses the request if this is not
    /// set, instead of selecting a wider process-global location.
    #[must_use]
    pub fn scratch(mut self, scratch: impl Into<PathBuf>) -> Self {
        self.scratch = Some(scratch.into());
        self
    }

    /// The environment the host is willing to pass on. Defaults to nothing.
    #[must_use]
    pub fn environment(mut self, environment: EnvSource) -> Self {
        self.environment = Some(environment);
        self
    }

    /// Who the host says it is. Required.
    #[must_use]
    pub fn client_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_info = Some(ClientInfo::new(name, version));
        self
    }

    /// A clock other than the operating system's.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The host's cancellation signal.
    #[must_use]
    pub fn cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// A policy that answers approvals without asking a person.
    ///
    /// Optional, and its absence is the safe default: every approval reaches the host.
    #[must_use]
    pub fn broker(mut self, broker: Arc<dyn PermissionBroker>) -> Self {
        self.broker = Some(broker);
        self
    }

    /// Caps other than the defaults.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Builds it.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when the launcher, the working directory or the client
    /// identity is missing. None of the three has a default a library could pick: a launcher is
    /// the host's sandbox policy, a working directory is an authorisation, and a client name is
    /// what a vendor logs.
    pub fn build(self) -> Result<HostContext> {
        let launcher = self.launcher.ok_or(Error::HostConfiguration {
            expected: "a ProcessLauncher",
            received: String::from("none"),
        })?;
        let cwd = self.cwd.ok_or(Error::HostConfiguration {
            expected: "an authorised working directory",
            received: String::from("none"),
        })?;
        let client_info = self.client_info.ok_or(Error::HostConfiguration {
            expected: "a ClientInfo naming the host",
            received: String::from("none"),
        })?;

        let limits = self.limits.unwrap_or_default();
        limits.approval_expires_at(SystemTime::UNIX_EPOCH)?;

        Ok(HostContext {
            launcher,
            cwd,
            scratch: self.scratch,
            environment: self.environment.unwrap_or_default(),
            client_info,
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            cancel: self.cancel.unwrap_or_default(),
            broker: self.broker,
            limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelToken, Clock, HostContext, Limits, SystemClock};
    use crate::env::EnvSource;
    use crate::error::{Error, Result};
    use crate::process::{LaunchSpec, ManagedProcess, ProcessLauncher};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    struct RefusingLauncher;

    #[async_trait::async_trait]
    impl ProcessLauncher for RefusingLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            Err(Error::Launch {
                program: spec.program().unwrap_or_default().to_owned(),
                message: String::from("a launcher that spawns nothing"),
            })
        }
    }

    fn context() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(RefusingLauncher))
            .cwd("/workspace")
            .client_info("mangostudio", "1.4.0")
            .environment(EnvSource::from_pairs([
                ("PATH", "/bin"),
                ("CONNECTOR_SECRET", "never-forward-this"),
            ]))
            .build()
            .expect("expected a context, received a refusal")
    }

    #[test]
    fn a_context_without_a_launcher_is_refused() {
        let error = HostContext::builder()
            .cwd("/workspace")
            .client_info("mangostudio", "1.4.0")
            .build()
            .expect_err("expected a refusal, received a context");
        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "a ProcessLauncher",
                    ..
                }
            ),
            "expected a launcher refusal, received {error:?}"
        );
    }

    #[test]
    fn a_context_without_an_authorised_directory_is_refused() {
        let error = HostContext::builder()
            .launcher(Arc::new(RefusingLauncher))
            .client_info("mangostudio", "1.4.0")
            .build()
            .expect_err("expected a refusal, received a context");
        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "an authorised working directory",
                    ..
                }
            ),
            "expected a directory refusal, received {error:?}"
        );
    }

    #[test]
    fn a_context_without_a_client_identity_is_refused() {
        let error = HostContext::builder()
            .launcher(Arc::new(RefusingLauncher))
            .cwd("/workspace")
            .build()
            .expect_err("expected a refusal, received a context");
        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "a ClientInfo naming the host",
                    ..
                }
            ),
            "expected a client-identity refusal, received {error:?}"
        );
    }

    #[test]
    fn a_context_rejects_an_approval_timeout_that_cannot_form_a_deadline() {
        let error = HostContext::builder()
            .launcher(Arc::new(RefusingLauncher))
            .cwd("/workspace")
            .client_info("mangostudio", "1.4.0")
            .limits(Limits {
                approval_timeout: Duration::MAX,
                ..Limits::default()
            })
            .build()
            .expect_err("expected Duration::MAX to be rejected before a deadline can panic");

        assert!(
            matches!(
                error,
                Error::HostConfiguration {
                    expected: "an approval deadline representable from the host clock",
                    ..
                }
            ),
            "expected an approval-timeout configuration refusal, received {error:?}"
        );
    }

    #[test]
    fn a_child_environment_is_the_allowlist_and_nothing_else() {
        let child = context().child_environment(&["VENDOR_CONFIG"]);
        assert_eq!(child.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(child.get("CONNECTOR_SECRET"), None);
    }

    #[test]
    fn scratch_is_opt_in_and_its_path_stays_out_of_debug_output() {
        let unset = context();
        assert_eq!(unset.scratch(), None);

        let configured = HostContext::builder()
            .launcher(Arc::new(RefusingLauncher))
            .cwd("/workspace/customer-secret")
            .scratch("/scratch/customer-secret")
            .client_info("client-secret-canary", "version-secret-canary")
            .build()
            .expect("expected a context with scratch storage");
        assert_eq!(
            configured.scratch(),
            Some(std::path::Path::new("/scratch/customer-secret"))
        );

        let rendered = format!("{configured:?}");
        assert!(rendered.contains("scratch_configured: true"));
        assert!(
            !rendered.contains("customer-secret") && !rendered.contains("secret-canary"),
            "expected host configuration to stay out of debug output, received {rendered}"
        );
        assert!(!format!("{:?}", configured.client_info()).contains("secret-canary"));
    }

    #[test]
    fn the_defaults_bound_a_turn_and_a_line() {
        let limits = Limits::default();
        assert_eq!(limits.turn_channel_capacity, 1_024);
        assert_eq!(limits.line.max_line_bytes, 1024 * 1024);
        assert_eq!(limits.kill_grace, Duration::from_secs(2));
        assert_eq!(context().limits(), &limits);
    }

    #[test]
    fn the_system_clock_moves_forward() {
        let before = SystemTime::now();
        let now = SystemClock.now();
        assert!(
            now >= before,
            "expected a time at or after {before:?}, received {now:?}"
        );
    }

    #[tokio::test]
    async fn a_cancel_token_wakes_every_clone() {
        let token = CancelToken::new();
        let watcher = token.clone();
        let waiting = tokio::spawn(async move { watcher.cancelled().await });

        token.cancel();
        waiting.await.expect("expected the waiter to wake");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn waiting_on_an_already_cancelled_token_returns_at_once() {
        let token = CancelToken::new();
        token.cancel();
        token.cancelled().await;
    }
}
