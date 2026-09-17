//! The harness itself: what is true before anything is spawned, and how a session is opened.
//!
//! Stateless and shareable, as the trait requires. It holds a descriptor and, when the host
//! resolved one, the path to the executable — nothing about any session, and no cached probe.

use std::sync::Arc;

use mango_external_agents::discovery::{AuthState, Discovery, GateVerdict, Model, ReasoningEffort};
use mango_external_agents::error::{Error, Result};
use mango_external_agents::harness::{
    Capabilities, Harness, HarnessDescriptor, HarnessKind, VendorInfo,
};
use mango_external_agents::jsonrpc::{Client, ClientOptions};
use mango_external_agents::permission::PermissionMatrix;
use mango_external_agents::process::LaunchSpec;
use mango_external_agents::session::{
    Configuration, OpenSession, ResumeMode, Session, SessionIds, SessionInfo,
    resume_fallback_reason,
};
use mango_external_agents::transport::{ExecutablePath, StdioSpec, TransportKind};
use mango_external_agents::transports::stdio;
use mango_external_agents::{ClientInfo as HostClientInfo, HostContext};

use crate::discovery::{self, LOGIN_HINT, PROGRAM};
use crate::permissions::PermissionOverrides;
use crate::protocol::requests::{
    AccountReadResponse, ClientInfo, InitializeParams, InitializeResponse, ModelListParams,
    ModelListResponse, ThreadResumeParams, ThreadStartParams, ThreadStartResponse, empty_params,
};
use crate::protocol::schema::MINIMUM_CODEX_VERSION;
use crate::protocol::{PEER_NAME, method};
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
/// Enabled features were checked against a running `codex app-server`. Host-supplied MCP
/// configuration is not implemented; servers configured in the user's own `config.toml` or with
/// `codex mcp` remain available to the vendor.
const CAPABILITIES: Capabilities = Capabilities {
    structured_streaming: true,
    reasoning_stream: true,
    interactive_approvals: true,
    resume: true,
    model_catalog: true,
    images: true,
    usage_reporting: true,
    cancellation: true,
    steering: true,
    session_listing: true,
    native_review: true,
    account_usage: true,
    mcp_passthrough: false,
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
    /// use mango_external_agents::{Harness, HarnessKind};
    ///
    /// let harness = CodexHarness::new();
    /// assert_eq!(harness.descriptor().kind, HarnessKind::Codex);
    /// assert!(!harness.descriptor().capabilities.mcp_passthrough);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptor: Arc::new(HarnessDescriptor {
                kind: HarnessKind::Codex,
                vendor: VENDOR,
                capabilities: CAPABILITIES,
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
        let version = match read_version(host, &self.executable).await {
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
                capabilities: Capabilities::none(),
                permission_matrix: self.permission_matrix(),
                models: Vec::new(),
            });
        }

        let (auth, models) = probe_app_server(host, &self.executable).await;
        Ok(Discovery {
            executable: self.executable.get().cloned(),
            version: Some(version),
            gate,
            auth,
            capabilities: CAPABILITIES,
            permission_matrix: self.permission_matrix(),
            models,
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        self.validate_open_session(&request)?;
        let executable = self.program_for(&request);
        let transport = stdio::open(
            host,
            &StdioSpec::new([PROGRAM, "app-server"]),
            &executable,
            VENDOR_ENVIRONMENT_KEYS,
        )
        .await?;

        let shared = Arc::new(Shared::new(host.clone(), request.session_id.clone()));
        let client = Arc::new(Client::connect(
            transport.link,
            CodexSession::handler(Arc::clone(&shared)),
            ClientOptions::new(PEER_NAME)
                // The app-server's own README: JSON-RPC 2.0 "with the `\"jsonrpc\":\"2.0\"` header
                // omitted on the wire".
                .without_version_header()
                .with_limits(host.limits()),
        ));

        // Everything from here can fail, and every failure has to take the child with it: a
        // half-opened session leaves a `codex app-server` running with nobody holding its handle.
        let opened = open_thread(host, &client, &request).await;
        let (info, thread_id) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let _ = client.close().await;
                let _ = transport
                    .control
                    .kill(mango_external_agents::CancelReason::Shutdown)
                    .await;
                return Err(error);
            }
        };

        shared.adopt_thread(thread_id);
        Ok(Box::new(CodexSession::new(
            info,
            shared,
            client,
            transport.control,
        )))
    }
}

/// Handshake, gate, auth and the thread itself.
async fn open_thread(
    host: &HostContext,
    client: &Client,
    request: &OpenSession,
) -> Result<(SessionInfo, String)> {
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

    // The build that is running has just named itself in the handshake, so the gate costs nothing
    // extra here. A user agent nobody could parse is not a refusal — the same reasoning as
    // `GateVerdict::Unknown` — so the session goes on.
    if let Some(version) = discovery::parse_user_agent_version(&handshake.user_agent)
        && discovery::meets_minimum(&version, MINIMUM_CODEX_VERSION) == Some(false)
    {
        return Err(Error::VersionGate {
            found: version,
            minimum: String::from(MINIMUM_CODEX_VERSION),
        });
    }

    let account: AccountReadResponse = client
        .request(method::ACCOUNT_READ, empty_params())
        .await
        .unwrap_or_default();
    if let AuthState::LoggedOut { login_hint } =
        discovery::auth_state(account.account.as_ref(), account.requires_openai_auth)
    {
        return Err(Error::AuthRequired { login_hint });
    }

    let configuration = &request.configuration;
    let vendor = crate::permissions::overrides(configuration);
    let cwd = host.cwd().to_string_lossy().into_owned();

    let (response, resumed, fallback_reason) = match &request.resume {
        Some(resume) => {
            let params = ThreadResumeParams {
                thread_id: resume.native_session_id.clone(),
                cwd: cwd.clone(),
                model: configuration.model.clone(),
                approval_policy: vendor.approval_policy,
                sandbox: vendor.sandbox,
                approvals_reviewer: vendor.approvals_reviewer,
                // Metadata only. The vendor keeps the transcript it wrote, and this library never
                // replays one into anybody's context.
                exclude_turns: true,
            };
            match client
                .request::<_, ThreadStartResponse>(method::THREAD_RESUME, params)
                .await
            {
                Ok(response) => (response, true, None),
                Err(error) if resume.mode == ResumeMode::Fallback => {
                    let reason = resume_fallback_reason(method::THREAD_RESUME, &error);
                    (
                        start_thread(client, &cwd, configuration, vendor).await?,
                        false,
                        Some(reason),
                    )
                }
                Err(error) => return Err(error),
            }
        }
        None => (
            start_thread(client, &cwd, configuration, vendor).await?,
            false,
            None,
        ),
    };

    let thread_id = response.thread.id.clone();
    Ok((
        SessionInfo {
            ids: SessionIds {
                session_id: request.session_id.clone(),
                native_session_id: thread_id.clone(),
            },
            resumed,
            fallback_reason,
            effective_configuration: Configuration {
                // What the vendor actually chose, which may not be what was asked for.
                model: response
                    .model
                    .clone()
                    .or_else(|| configuration.model.clone()),
                effort: response
                    .reasoning_effort
                    .clone()
                    .or_else(|| configuration.effort.clone()),
                level: configuration.level,
                routing: configuration.routing,
            },
            capabilities: CAPABILITIES,
        },
        thread_id,
    ))
}

async fn start_thread(
    client: &Client,
    cwd: &str,
    configuration: &Configuration,
    vendor: PermissionOverrides,
) -> Result<ThreadStartResponse> {
    client
        .request(
            method::THREAD_START,
            ThreadStartParams {
                cwd: cwd.to_owned(),
                model: configuration.model.clone(),
                approval_policy: vendor.approval_policy,
                sandbox: vendor.sandbox,
                approvals_reviewer: vendor.approvals_reviewer,
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
async fn read_version(host: &HostContext, executable: &ExecutablePath) -> Option<String> {
    let mut child = host
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
        .ok()?;

    let mut lines = mango_external_agents::process::LineStream::new(
        std::mem::replace(&mut child.stdout, Box::new(NoBytes)),
        host.limits().line,
    );
    // Bounded, because a probe must end. `codex --version` prints one line and exits, but a
    // binary that is not the one the host thinks it is may print nothing and sit there — and a
    // probe that waited on it would hang whatever called it, with no turn to cancel.
    let first = tokio::time::timeout(host.limits().request_timeout, lines.next_line()).await;
    let _ = child
        .control
        .kill(mango_external_agents::CancelReason::Shutdown)
        .await;
    discovery::parse_version(&first.ok()?.ok().flatten()?)
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
) -> (AuthState, Vec<Model>) {
    let unknown = (AuthState::Unknown, Vec::new());
    let Ok(transport) = stdio::open(
        host,
        &StdioSpec::new([PROGRAM, "app-server"]),
        executable,
        VENDOR_ENVIRONMENT_KEYS,
    )
    .await
    else {
        return unknown;
    };

    let client = Client::connect(
        transport.link,
        Arc::new(Silent),
        ClientOptions::new(PEER_NAME)
            .without_version_header()
            .with_limits(host.limits()),
    );

    let handshake: Result<InitializeResponse> = client
        .request(
            method::INITIALIZE,
            InitializeParams {
                client_info: client_info(host.client_info()),
                capabilities: None,
            },
        )
        .await;
    let probed = if handshake.is_ok() {
        let _ = client.notify(method::INITIALIZED, empty_params()).await;
        let account: AccountReadResponse = client
            .request(method::ACCOUNT_READ, empty_params())
            .await
            .unwrap_or_default();
        let models: ModelListResponse = client
            .request(method::MODEL_LIST, ModelListParams { limit: None })
            .await
            .unwrap_or_default();
        (
            discovery::auth_state(account.account.as_ref(), account.requires_openai_auth),
            models
                .data
                .into_iter()
                .filter(|model| !model.hidden)
                .map(to_model)
                .collect(),
        )
    } else {
        unknown
    };

    let _ = client.close().await;
    let _ = transport
        .control
        .kill(mango_external_agents::CancelReason::Shutdown)
        .await;
    probed
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
