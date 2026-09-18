//! The harness itself: what is true before anything is spawned, and how a session is opened.
//!
//! Stateless and shareable, as the trait requires. It holds a descriptor and, when the host
//! resolved one, the path to the executable — nothing about any session, and no cached probe.

use std::collections::BTreeMap;
use std::sync::Arc;

use mango_external_agents::Dispatch;
use mango_external_agents::configuration::{Configuration, ConfigurationCatalog};
use mango_external_agents::discovery::{AuthState, Discovery, GateVerdict, Model, ReasoningEffort};
use mango_external_agents::error::{Error, Result};
use mango_external_agents::harness::{
    Capabilities, CapabilityCeiling, DiscoveredCapabilities, Harness, HarnessDescriptor, VendorInfo,
};
use mango_external_agents::identity::HarnessIdentity;
use mango_external_agents::jsonrpc::{Client, ClientOptions};
use mango_external_agents::permission::PermissionMatrix;
use mango_external_agents::process::{LaunchSpec, ProcessCleanupGuard};
use mango_external_agents::session::{
    OpenSession, ResumeMode, Session, SessionIds, SessionPage, SessionQuery, resume_fallback_reason,
};
use mango_external_agents::state::{SessionSnapshot, SessionState, TransportSelection};
use mango_external_agents::transport::{ExecutablePath, StdioSpec, TransportKind};
use mango_external_agents::transports::stdio;
use mango_external_agents::{ClientInfo as HostClientInfo, HostContext};

use crate::discovery::{self, LOGIN_HINT, PROGRAM};
use crate::permissions::PermissionOverrides;
use crate::protocol::requests::{
    AccountReadResponse, ApprovalsReviewer, ClientInfo, InitializeParams, InitializeResponse,
    ModelListParams, ModelListResponse, ThreadReadParams, ThreadReadResponse, ThreadResumeParams,
    ThreadStartParams, ThreadStartResponse, ThreadSummary, empty_params,
};
use crate::protocol::schema::MINIMUM_CODEX_VERSION;
use crate::protocol::{CODE_PREFIX, PEER_NAME, method};
use crate::session::{CodexSession, Shared};

/// Who owns the CLI this harness drives.
///
/// Nominative use only: the name identifies the tool being launched. The two documents are what a
/// host's own disclosure links, carried verbatim rather than summarised — paraphrasing another
/// company's terms would be making a claim about their obligations.
const VENDOR: VendorInfo = VendorInfo {
    company: "OpenAI",
    terms_url: "https://openai.com/policies/terms-of-use/",
    privacy_url: "https://openai.com/policies/privacy-policy/",
    // Codex reads skills into a prompt section rather than registering them under `/`, so offering
    // one as a slash command would advertise a command the CLI never loaded. Probed against the
    // CLI, not inferred from its docs.
    skills_are_slash_commands: false,
};

/// The environment variables this harness lets through to a Codex child.
///
/// One, and it is a directory rather than a credential: `CODEX_HOME` is where the user's own CLI
/// keeps its configuration, and a child that could not find it would run under a configuration the
/// user never chose. `OPENAI_API_KEY` is deliberately absent — this library does no API-key
/// plumbing of any kind, and a host that wants one sets it up with the vendor's own CLI.
const VENDOR_ENVIRONMENT_KEYS: &[&str] = &["CODEX_HOME"];

/// What this harness could support, given a new enough CLI.
///
/// Enabled features were checked against a running `codex app-server`. Host-supplied MCP entries
/// use the documented per-thread `config` override, which does not edit the user's config file.
const CAPABILITIES: Capabilities = Capabilities {
    structured_streaming: true,
    reasoning_stream: true,
    interactive_approvals: true,
    // Neither is implemented yet: the app-server has no documented question surface distinct from
    // an approval, and settings can only be changed by opening a new turn, not mid-session.
    questions: false,
    resume: true,
    model_catalog: true,
    // Not enumerated: `mea capture` has not yet been run against a settings-listing surface, and an
    // empty catalog is the honest answer until one is.
    configuration_catalog: false,
    session_configuration: false,
    images: true,
    usage_reporting: true,
    cancellation: true,
    steering: true,
    session_listing: true,
    native_review: true,
    account_usage: true,
    mcp_passthrough: true,
    configuration: true,
};

/// The transports this harness accepts.
///
/// `stdio` only. The app-server also offers `--listen ws://` and a unix socket, and its own README
/// says of the first: "Websocket transport is currently experimental and unsupported. Do not rely
/// on it for production workloads." A harness that declared it would be inviting hosts to build on
/// a surface the vendor has already withdrawn once.
const TRANSPORTS: &[TransportKind] = &[TransportKind::Stdio];

/// The OpenAI Codex harness.
#[derive(Clone, Debug)]
pub struct CodexHarness {
    descriptor: Arc<HarnessDescriptor>,
    executable: ExecutablePath,
}

impl Default for CodexHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexHarness {
    /// A harness that spawns whatever `codex` the host's launcher resolves.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::CodexHarness;
    /// use mango_external_agents::{Harness, HarnessId};
    ///
    /// let harness = CodexHarness::new();
    /// assert_eq!(harness.descriptor().id(), &HarnessId::codex());
    /// assert!(harness.descriptor().capabilities.capabilities().mcp_passthrough);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptor: Arc::new(HarnessDescriptor {
                identity: HarnessIdentity::codex(),
                vendor: VENDOR,
                capabilities: CapabilityCeiling::new(CAPABILITIES),
                transports: TRANSPORTS,
                vendor_environment_keys: VENDOR_ENVIRONMENT_KEYS,
            }),
            executable: ExecutablePath::default(),
        }
    }

    /// A harness that spawns this executable when a request does not name one.
    ///
    /// The host owns resolution — it knows the toolchain, the version manager and the sandbox the
    /// child runs under — and [`Harness::probe`] has no request to carry a path on, so this is
    /// where a probe learns which binary to ask.
    #[must_use]
    pub fn with_executable(mut self, executable: ExecutablePath) -> Self {
        self.executable = executable;
        self
    }

    /// The executable this session should spawn: the request's, then the harness's.
    fn program_for(&self, request: &OpenSession) -> ExecutablePath {
        request
            .executable
            .get()
            .cloned()
            .map_or_else(|| self.executable.clone(), ExecutablePath::resolved)
    }
}

#[async_trait::async_trait]
impl Harness for CodexHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    fn permission_matrix(&self) -> PermissionMatrix {
        crate::permissions::matrix()
    }

    async fn probe(&self, host: &HostContext) -> Result<Discovery> {
        let version = match read_version(host, &self.executable).await? {
            Some(version) => version,
            // Nothing answered `--version`, so nothing is installed as far as a probe can tell. An
            // executable the host resolved but cannot run is the same fact to a caller.
            None => return Ok(Discovery::not_installed()),
        };
        let gate = discovery::gate(Some(&version), MINIMUM_CODEX_VERSION);

        // A build that would be refused is not worth a handshake: the app-server is a process, and
        // spawning one to ask an account question whose answer cannot be acted on is a cost with
        // no buyer.
        if !matches!(gate, GateVerdict::Usable) {
            return Ok(Discovery {
                executable: self.executable.get().cloned(),
                version: Some(version),
                gate,
                auth: AuthState::Unknown,
                capabilities: DiscoveredCapabilities::none(),
                permission_matrix: self.permission_matrix(),
                models: Vec::new(),
                configuration_catalog: ConfigurationCatalog::empty(),
            });
        }

        let (auth, models) = probe_app_server(host, &self.executable).await?;
        Ok(Discovery {
            executable: self.executable.get().cloned(),
            version: Some(version),
            gate,
            auth,
            capabilities: DiscoveredCapabilities::new(CAPABILITIES),
            permission_matrix: self.permission_matrix(),
            models,
            configuration_catalog: ConfigurationCatalog::empty(),
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        self.validate_open_session(host, &request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        mango_external_agents::configuration::refuse_unsupported_native(&request.configuration)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        // No driven surface drops a host override back to config.toml defaults.
        mango_external_agents::configuration::refuse_unsupported_reset(&request.configuration)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let mcp_config =
            crate::configuration::thread_override(&request.configuration, &request.mcp_servers)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let effective_transport = self
            .descriptor()
            .resolve_transport(request.transport)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let cwd = host
            .absolute_cwd()
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let executable = self.program_for(&request);
        let transport = stdio::open(
            host,
            &StdioSpec::new([PROGRAM, "app-server"]),
            &executable,
            VENDOR_ENVIRONMENT_KEYS,
        )
        .await?;

        let cleanup = ProcessCleanupGuard::new(
            transport.control,
            *host.limits(),
            mango_external_agents::CancelReason::Shutdown,
        );
        let shared = Arc::new(Shared::new(host.clone(), request.session_id.clone()));
        let client = Arc::new(Client::connect(
            transport.link,
            CodexSession::handler(Arc::clone(&shared)),
            ClientOptions::new(PEER_NAME)
                .with_code_prefix(CODE_PREFIX)
                // The app-server's own README: JSON-RPC 2.0 "with the `\"jsonrpc\":\"2.0\"` header
                // omitted on the wire".
                .without_version_header()
                .with_limits(host.limits()),
        ));

        // Everything from here can fail, and every failure has to take the child with it: a
        // half-opened session leaves a `codex app-server` running with nobody holding its handle.
        let opened = open_thread(
            host,
            &client,
            &request,
            effective_transport,
            mcp_config,
            cwd,
        )
        .await;
        let (state, thread_id) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let _ = client.close().await;
                return match cleanup.finish().await {
                    Ok(_) => Err(error),
                    Err(cleanup_error) => Err(cleanup_error),
                };
            }
        };

        shared.adopt_thread(thread_id);
        Ok(Box::new(CodexSession::new(
            state,
            shared,
            client,
            cleanup.into_control(),
        )))
    }

    async fn list_native_sessions(
        &self,
        host: &HostContext,
        query: SessionQuery,
    ) -> Result<SessionPage> {
        crate::session::validate_list_workspace(host, &query)?;
        let connection = ProbeConnection::open(host, &self.executable).await?;
        let page = crate::session::list_threads(&connection.client, host, query).await;
        match connection.close().await {
            Ok(()) => page,
            Err(error) => Err(error),
        }
    }
}

/// Handshake, gate, auth and the thread itself.
async fn open_thread(
    host: &HostContext,
    client: &Client,
    request: &OpenSession,
    effective_transport: TransportKind,
    mcp_config: Option<BTreeMap<String, serde_json::Value>>,
    cwd: &str,
) -> Result<(SessionState, String)> {
    let handshake: InitializeResponse = client
        .request(
            method::INITIALIZE,
            InitializeParams {
                client_info: client_info(host.client_info()),
                capabilities: None,
            },
        )
        .await?;
    client.notify(method::INITIALIZED, empty_params()).await?;

    require_supported_handshake_version(&handshake)?;

    let account: AccountReadResponse = client
        .request(method::ACCOUNT_READ, empty_params())
        .await
        .unwrap_or_default();
    if let AuthState::LoggedOut { login_hint } =
        discovery::auth_state(account.account.as_ref(), account.requires_openai_auth)
    {
        return Err(Error::AuthRequired { login_hint });
    }

    let vendor = crate::permissions::overrides(&request.configuration);
    let requested_model = request.configuration.model.set_value().cloned();

    let (response, resumed, fallback_reason) = match &request.resume {
        Some(resume) => {
            let workspace_verified =
                preflight_resume_workspace(client, &resume.native_session_id, cwd).await?;
            let params = ThreadResumeParams {
                thread_id: resume.native_session_id.clone(),
                cwd: cwd.to_owned(),
                model: requested_model.clone(),
                approval_policy: vendor.approval_policy,
                sandbox: vendor.sandbox,
                approvals_reviewer: vendor.approvals_reviewer,
                config: mcp_config.clone(),
                // Metadata only. The vendor keeps the transcript it wrote, and this library never
                // replays one into anybody's context.
                exclude_turns: true,
            };
            match client
                .request::<_, ThreadStartResponse>(method::THREAD_RESUME, params)
                .await
            {
                Ok(response) if workspace_verified => (response, true, None),
                Ok(_) => {
                    return Err(Error::HostConfiguration {
                        expected: "a Codex thread in the host's authorized workspace",
                        received: String::from(
                            "thread/read could not verify the original workspace",
                        ),
                    });
                }
                Err(error)
                    if resume.mode == ResumeMode::Fallback
                        && missing_rollout(&error, &resume.native_session_id) =>
                {
                    let reason = resume_fallback_reason(method::THREAD_RESUME, &error);
                    (
                        start_thread(client, cwd, &requested_model, vendor, &mcp_config).await?,
                        false,
                        Some(reason),
                    )
                }
                Err(error) => return Err(error),
            }
        }
        None => (
            start_thread(client, cwd, &requested_model, vendor, &mcp_config).await?,
            false,
            None,
        ),
    };

    authorize_thread(
        &response.thread,
        request
            .resume
            .as_ref()
            .filter(|_| resumed)
            .map(|resume| resume.native_session_id.as_str()),
        cwd,
    )?;
    let thread_id = response.thread.id.clone();
    let ids = SessionIds {
        session_id: request.session_id.clone(),
        native_session_id: thread_id.clone(),
    };

    // What this harness really put on the wire, after the app-server accepted the thread.
    let mut accepted = Configuration::unknown();
    if let Some(model) = &requested_model {
        accepted = accepted.with_model(model.clone());
    }
    if let Some(effort) = request.configuration.effort.set_value() {
        accepted = accepted.with_effort(effort.clone());
    }
    if let Some(level) = request.configuration.level.set_value() {
        accepted = accepted.with_level(*level);
    }
    if let Some(routing) = request.configuration.routing.set_value() {
        accepted = accepted.with_routing(*routing);
    }

    // What the vendor itself reported back. `approvalsReviewer` is unambiguous evidence of
    // routing; `approvalPolicy` alone cannot be read back into a `PermissionLevel`, because the
    // same policy pairs with more than one sandbox in this harness's own table — so `level` stays
    // unknown here rather than guessing.
    let mut observed = Configuration::unknown();
    if let Some(model) = &response.model {
        observed = observed.with_model(model.clone());
    }
    if let Some(effort) = &response.reasoning_effort {
        observed = observed.with_effort(effort.clone());
    }
    if let Some(reviewer) = response.approvals_reviewer {
        observed = observed.with_routing(match reviewer {
            ApprovalsReviewer::User => mango_external_agents::ApprovalRouting::User,
            ApprovalsReviewer::AutoReview => mango_external_agents::ApprovalRouting::AutoReview,
        });
    }

    let configuration_state = mango_external_agents::configuration::ConfigurationState::new(
        request.configuration.requested(),
        accepted,
        observed,
    );

    let mut snapshot = SessionSnapshot::opening(
        ids,
        HarnessIdentity::codex(),
        TransportSelection::new(request.transport, effective_transport),
        host.now(),
    )
    .with_capabilities(mango_external_agents::harness::SessionCapabilities::new(
        CAPABILITIES,
    ))
    .with_configuration(configuration_state)
    .with_catalog(ConfigurationCatalog::empty());
    if resumed {
        snapshot = snapshot.resumed();
    }
    if let Some(reason) = fallback_reason {
        snapshot = snapshot.with_fallback_reason(reason);
    }

    Ok((
        SessionState::new(std::sync::Arc::clone(host.clock()), snapshot),
        thread_id,
    ))
}

/// Refuses an app-server that identified itself below this harness's pinned protocol floor.
fn require_supported_handshake_version(handshake: &InitializeResponse) -> Result<()> {
    // The build that is running has just named itself in the handshake, so the gate costs nothing
    // extra here. A user agent nobody could parse is not a refusal — the same reasoning as
    // `GateVerdict::Unknown` — so the connection goes on.
    if let Some(version) = discovery::parse_user_agent_version(&handshake.user_agent)
        && discovery::meets_minimum(&version, MINIMUM_CODEX_VERSION) == Some(false)
    {
        return Err(Error::VersionGate {
            found: version,
            minimum: String::from(MINIMUM_CODEX_VERSION),
        });
    }
    Ok(())
}

/// The pinned app-server uses this exact invalid-request response when its thread store has no
/// rollout for the requested id. The same JSON-RPC code also covers configuration failures, so
/// matching the code alone would create a different conversation after an unrelated refusal.
fn missing_rollout(error: &Error, native_session_id: &str) -> bool {
    matches!(error.cause(), Error::Vendor(vendor)
        if vendor.vendor_code.as_deref() == Some("-32600")
            && vendor.message == format!("no rollout found for thread id {native_session_id}"))
}

/// Proves a native thread's original workspace before loading it for a resumed conversation.
/// A missing read is left unverified; only the resume method can prove that fallback is safe.
async fn preflight_resume_workspace(
    client: &Client,
    native_session_id: &str,
    cwd: &str,
) -> Result<bool> {
    let read = client
        .request::<_, ThreadReadResponse>(
            method::THREAD_READ,
            ThreadReadParams {
                thread_id: native_session_id.to_owned(),
                include_turns: false,
            },
        )
        .await;
    match read {
        Ok(read) => {
            authorize_thread(&read.thread, Some(native_session_id), cwd)?;
            Ok(true)
        }
        Err(error) if thread_not_loaded(&error, native_session_id) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The pinned metadata-only read's missing-thread response, never a generic RPC failure.
fn thread_not_loaded(error: &Error, native_session_id: &str) -> bool {
    matches!(error.cause(), Error::Vendor(vendor)
        if vendor.vendor_code.as_deref() == Some("-32600")
            && vendor.message == format!("thread not loaded: {native_session_id}"))
}

/// Rejects a different identity or workspace before a handle can be exposed to the host.
fn authorize_thread(thread: &ThreadSummary, expected_id: Option<&str>, cwd: &str) -> Result<()> {
    if let Some(expected_id) = expected_id
        && thread.id != expected_id
    {
        return Err(Error::HostConfiguration {
            expected: "the requested Codex thread id",
            received: String::from("a different native thread id"),
        });
    }
    if thread.cwd.as_deref() != Some(cwd) {
        return Err(Error::HostConfiguration {
            expected: "a Codex thread in the host's authorized workspace",
            received: String::from("a missing or different workspace"),
        });
    }
    mango_external_agents::normalize::opaque_id(&thread.id, "native thread id")?;
    Ok(())
}

async fn start_thread(
    client: &Client,
    cwd: &str,
    model: &Option<String>,
    vendor: PermissionOverrides,
    mcp_config: &Option<BTreeMap<String, serde_json::Value>>,
) -> Result<ThreadStartResponse> {
    client
        .request(
            method::THREAD_START,
            ThreadStartParams {
                cwd: cwd.to_owned(),
                model: model.clone(),
                approval_policy: vendor.approval_policy,
                sandbox: vendor.sandbox,
                approvals_reviewer: vendor.approvals_reviewer,
                config: mcp_config.clone(),
            },
        )
        .await
}

/// The host's own identity, as the app-server's `clientInfo`.
fn client_info(host: &HostClientInfo) -> ClientInfo {
    ClientInfo {
        name: host.name.clone(),
        title: None,
        version: host.version.clone(),
    }
}

/// Whatever `codex --version` printed, or nothing when it would not run.
async fn read_version(host: &HostContext, executable: &ExecutablePath) -> Result<Option<String>> {
    host.absolute_cwd()?;
    let mut child = match host
        .launcher()
        .spawn(LaunchSpec {
            argv: vec![
                executable.or(String::from(PROGRAM)),
                String::from("--version"),
            ],
            cwd: host.cwd().to_path_buf(),
            env: host.child_environment(VENDOR_ENVIRONMENT_KEYS),
            stdin: false,
            hide_window: true,
        })
        .await
    {
        Ok(child) => child,
        Err(error) if error.cleanup_control().is_some() => return Err(error),
        Err(_) => return Ok(None),
    };

    let mut lines = mango_external_agents::process::LineStream::new(
        std::mem::replace(&mut child.stdout, Box::new(NoBytes)),
        host.limits().line,
    );
    let cleanup = ProcessCleanupGuard::new(
        child.control,
        *host.limits(),
        mango_external_agents::CancelReason::Shutdown,
    );
    // Bounded, because a probe must end. `codex --version` prints one line and exits, but a
    // binary that is not the one the host thinks it is may print nothing and sit there — and a
    // probe that waited on it would hang whatever called it, with no turn to cancel.
    let first = tokio::time::timeout(host.limits().request_timeout, lines.next_line()).await;
    cleanup.finish().await?;
    Ok(first
        .ok()
        .and_then(|line| line.ok().flatten())
        .and_then(|line| discovery::parse_version(&line)))
}

/// A stand-in for a byte source that has been taken, so the child struct stays whole.
struct NoBytes;

#[async_trait::async_trait]
impl mango_external_agents::process::ByteSource for NoBytes {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// Auth state and the model catalog, from one short-lived app-server connection.
///
/// A probe that could not reach the server reports `Unknown` and no models rather than failing:
/// the CLI is installed and the gate passed, and how a host treats an unreachable app-server is
/// its own decision.
async fn probe_app_server(
    host: &HostContext,
    executable: &ExecutablePath,
) -> Result<(AuthState, Vec<Model>)> {
    let unknown = (AuthState::Unknown, Vec::new());
    let connection = match ProbeConnection::open(host, executable).await {
        Ok(connection) => connection,
        Err(error) if error.cleanup_control().is_some() => return Err(error),
        Err(_) => return Ok(unknown),
    };

    let account: AccountReadResponse = connection
        .client
        .request(method::ACCOUNT_READ, empty_params())
        .await
        .unwrap_or_default();
    let models: ModelListResponse = connection
        .client
        .request(method::MODEL_LIST, ModelListParams { limit: None })
        .await
        .unwrap_or_default();
    let probed = (
        discovery::auth_state(account.account.as_ref(), account.requires_openai_auth),
        models
            .data
            .into_iter()
            .filter(|model| !model.hidden)
            .map(to_model)
            .collect(),
    );
    match connection.close().await {
        Ok(()) => Ok(probed),
        Err(error) if error.cleanup_control().is_some() => Err(error),
        Err(_) => Ok(unknown),
    }
}

/// One bounded app-server connection for pre-conversation read-only services.
struct ProbeConnection {
    client: Client,
    cleanup: ProcessCleanupGuard,
}

impl ProbeConnection {
    async fn open(host: &HostContext, executable: &ExecutablePath) -> Result<Self> {
        host.absolute_cwd()?;
        let transport = stdio::open(
            host,
            &StdioSpec::new([PROGRAM, "app-server"]),
            executable,
            VENDOR_ENVIRONMENT_KEYS,
        )
        .await?;
        let client = Client::connect(
            transport.link,
            Arc::new(Silent),
            ClientOptions::new(PEER_NAME)
                .with_code_prefix(CODE_PREFIX)
                .without_version_header()
                .with_limits(host.limits()),
        );
        let connection = Self {
            client,
            cleanup: ProcessCleanupGuard::new(
                transport.control,
                *host.limits(),
                mango_external_agents::CancelReason::Shutdown,
            ),
        };
        let handshake: Result<InitializeResponse> = connection
            .client
            .request(
                method::INITIALIZE,
                InitializeParams {
                    client_info: client_info(host.client_info()),
                    capabilities: None,
                },
            )
            .await;
        let result = match handshake {
            Ok(handshake) => connection
                .client
                .notify(method::INITIALIZED, empty_params())
                .await
                .and_then(|()| require_supported_handshake_version(&handshake)),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            return match connection.close().await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        Ok(connection)
    }

    async fn close(self) -> Result<()> {
        let client = self.client.close().await;
        match self.cleanup.finish().await {
            Ok(_) => client,
            Err(error) => Err(error),
        }
    }
}

fn to_model(model: crate::protocol::requests::Model) -> Model {
    Model {
        id: model.id,
        display_name: model.display_name,
        description: model.description,
        is_default: model.is_default,
        reasoning_efforts: model
            .supported_reasoning_efforts
            .into_iter()
            .map(|effort| ReasoningEffort {
                id: effort.reasoning_effort,
                display_name: None,
                description: effort.description,
            })
            .collect(),
        default_reasoning_effort: model.default_reasoning_effort,
    }
}

/// A handler for a connection nobody is streaming: a probe asks and leaves.
struct Silent;

#[async_trait::async_trait]
impl mango_external_agents::jsonrpc::PeerHandler for Silent {
    async fn on_notification(&self, _method: String, _params: serde_json::Value) {}

    async fn on_request(
        &self,
        _method: String,
        _params: serde_json::Value,
        _id: mango_external_agents::jsonrpc::RequestId,
    ) -> mango_external_agents::jsonrpc::ServerRequestOutcome {
        // A probe is not a session: there is nobody to put a question to, and the server must not
        // be left waiting on one.
        mango_external_agents::jsonrpc::ServerRequestOutcome::Failure(
            mango_external_agents::jsonrpc::JsonRpcError {
                code: -32601,
                message: String::from(
                    "expected a probe that only asks, received a question; this connection has \
                     nobody to answer it",
                ),
                data: None,
            },
        )
    }
}

/// The vendor's own login command, re-exported where a host looking at the harness will find it.
pub const CODEX_LOGIN_HINT: &str = LOGIN_HINT;
