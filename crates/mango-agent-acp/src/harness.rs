//! One [`Harness`] per profile, and what a probe of this machine can honestly say.
//!
//! # What the probe does, and what it refuses to do
//!
//! It runs the profile's version argv through the host's launcher and reads what the agent printed.
//! It does not search `PATH` — the core is explicit that the library never does, and a host that
//! resolved a path passes it on [`OpenSession::with_executable`] — and it does not run `initialize`,
//! because a handshake is a session, and discovering an agent should not open one.
//!
//! [`AuthState`] is therefore always [`AuthState::Unknown`]. ACP's `initialize` reports which
//! authentication *methods* an agent offers and has no field anywhere for whether somebody is signed
//! in; the only way to find out is to try `session/new` and see. So the harness does not guess at
//! discovery, and a signed-out agent surfaces where it actually becomes known: `session/new`
//! answering `-32000`, which [`AcpHarness::open_session`] turns into
//! [`Error::AuthRequired`] carrying the profile's own
//! documented login command.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/initialization>

use std::sync::Arc;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, ClientCapabilities, FileSystemCapabilities, Implementation,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, SessionId as AcpSessionId,
    SessionModeState,
};
use mango_external_agents::permission::PermissionMatrix;
use mango_external_agents::session::{
    Configuration, OpenSession, ResumeMode, Session, SessionIds, SessionInfo,
};
use mango_external_agents::transport::TransportKind;
use mango_external_agents::{
    AcpSpec, AuthState, Capabilities, Discovery, Error, GateVerdict, Harness, HarnessDescriptor,
    HarnessKind, HostContext, LaunchSpec, LineStream, Result, StdioSpec,
};

use crate::client::{self, SessionState};
use crate::profile::{AcpProfile, matrix};
use crate::session::{AcpSession, refuse_model_selection};
use crate::transport;
use crate::version::{self, Comparison};

/// The transports an ACP agent is driven over.
///
/// One, in 0.1. [`AcpSpec::Http`] exists in the core and this crate does not carry it: the official
/// `agent-client-protocol-http` crate pulls `aws-lc-rs` through `reqwest` and `async-tungstenite`,
/// and this workspace's `deny.toml` bans it — TLS is `ring` everywhere. An HTTP carrier lands when
/// that crate can be built on `ring`, and until then asking for one is
/// [`Error::UnsupportedTransport`](mango_external_agents::Error::UnsupportedTransport) rather than a
/// dependency nobody can release.
const TRANSPORTS: &[TransportKind] = &[TransportKind::Acp];

/// The ceiling of what this harness could reach given a new enough agent.
///
/// Read against the capabilities ACP v1 actually has:
///
/// * **No steering.** `session/prompt` is one request per turn and there is no surface for adding to
///   one that is running, so [`Session::steer`](mango_external_agents::Session::steer) stays the
///   trait's typed refusal on every profile.
/// * **No model catalog.** `session/new` takes a working directory and MCP servers. See
///   [`refuse_model_selection`].
/// * **No native review and no account usage.** Neither exists on the v1 surface. `usage_update`
///   reports a session's context window, which is thread usage rather than plan quota.
/// * **No MCP passthrough yet.** Host-supplied servers are refused before launch. The harness sends
///   an empty `session/new.mcpServers` list.
fn ceiling() -> Capabilities {
    Capabilities {
        structured_streaming: true,
        reasoning_stream: true,
        interactive_approvals: true,
        resume: true,
        images: true,
        usage_reporting: true,
        cancellation: true,
        session_listing: true,
        ..Capabilities::none()
    }
}

/// A [`Harness`] for one ACP agent.
///
/// One instance per profile: a [`HarnessKind::Acp`] names the profile, so a host that drives Cursor
/// and OpenCode registers two of these in its [`HarnessRegistry`](mango_external_agents::HarnessRegistry).
///
/// # Example
///
/// ```
/// use mango_agent_acp::AcpHarness;
/// use mango_external_agents::{Harness, HarnessKind};
///
/// let harness = AcpHarness::builtin("opencode").expect("a built-in profile");
/// assert_eq!(harness.descriptor().kind.to_string(), "acp:opencode");
/// ```
#[derive(Debug)]
pub struct AcpHarness {
    profile: Arc<AcpProfile>,
    descriptor: HarnessDescriptor,
}

impl AcpHarness {
    /// A harness for this profile.
    pub fn new(profile: Arc<AcpProfile>) -> Self {
        let descriptor = HarnessDescriptor {
            kind: HarnessKind::Acp(profile.id.clone()),
            vendor: profile.vendor,
            capabilities: ceiling(),
            transports: TRANSPORTS,
            vendor_environment_keys: profile.vendor_environment_keys,
        };
        Self {
            profile,
            descriptor,
        }
    }

    /// A harness for one of the profiles this crate ships, by id.
    pub fn builtin(id: &str) -> Option<Self> {
        crate::profile::builtin_profile(id).map(Self::new)
    }

    /// One harness per built-in profile, ready to register.
    pub fn builtins() -> Vec<Arc<dyn Harness>> {
        crate::profile::builtin_profiles()
            .into_iter()
            .map(|profile| Arc::new(Self::new(profile)) as Arc<dyn Harness>)
            .collect()
    }

    /// The profile this harness drives.
    pub fn profile(&self) -> &Arc<AcpProfile> {
        &self.profile
    }

    /// What the agent's own capabilities mean in the neutral vocabulary.
    ///
    /// Bounded by [`ceiling`] on the way out by
    /// [`Harness::discover`](mango_external_agents::Harness::discover)'s own normalisation, so a
    /// reading that drifted wider than the descriptor is caught rather than reported.
    fn capabilities_from(agent: &AgentCapabilities) -> Capabilities {
        Capabilities {
            resume: agent.load_session,
            images: agent.prompt_capabilities.image,
            session_listing: agent.session_capabilities.list.is_some(),
            ..ceiling()
        }
    }

    /// The client capabilities this harness advertises.
    ///
    /// Everything declined. The host owns files and terminals, so an agent that asked this client to
    /// read or write one would be asking the library to act on the host's filesystem on a third
    /// party's instruction — and a vendor tool call must never reach a host's executor. Declining is
    /// not a gap: every agent here has its own file and shell tools and uses them, which is what the
    /// activity events describe.
    fn client_capabilities() -> ClientCapabilities {
        ClientCapabilities::default()
            .fs(FileSystemCapabilities::default())
            .terminal(false)
    }
}

#[async_trait::async_trait]
impl Harness for AcpHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    fn permission_matrix(&self) -> PermissionMatrix {
        matrix(&self.profile.modes)
    }

    async fn probe(&self, host: &HostContext) -> Result<Discovery> {
        let argv = self.profile.version_argv.clone();
        if argv.is_empty() {
            return Ok(Discovery::not_installed());
        }
        let printed = match run_version(host, &self.descriptor, argv).await {
            Ok(printed) => printed,
            // A launcher that could not start it is the definition of not installed here: the
            // library does not search `PATH`, so "no such program" is what it learns instead.
            Err(Error::Launch { .. }) => return Ok(Discovery::not_installed()),
            Err(error) => return Err(error),
        };

        let version = version::parse(&printed);
        Ok(Discovery {
            executable: None,
            version: version.clone(),
            gate: gate(version.as_deref(), self.profile.minimum_version.as_deref()),
            // ACP has no signed-in surface; see this module's own documentation.
            auth: AuthState::Unknown,
            // What this *build* supports is only knowable from `initialize`, which a probe does not
            // run. The ceiling is what the harness could reach; `open_session` reports what the
            // agent actually advertised on `SessionInfo::capabilities`.
            capabilities: ceiling(),
            models: Vec::new(),
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        refuse_model_selection(&request.configuration)?;
        let matrix = self.permission_matrix();
        if request.configuration.level.is_some_and(|level| {
            !matrix.supports(
                level,
                request
                    .configuration
                    .routing
                    .unwrap_or(mango_external_agents::ApprovalRouting::User),
            )
        }) {
            // Refused, never downgraded. Silently running a read-only request under "ask every time"
            // would grant more freedom than anybody chose, which is the one direction a permission
            // mistake must not go.
            return Err(Error::HostConfiguration {
                expected: "a (level, routing) pair this profile supports",
                received: format!(
                    "{:?}/{:?} on {}",
                    request.configuration.level, request.configuration.routing, self.profile.id
                ),
        if !request.mcp_servers.is_empty() {
            return Err(Error::HostConfiguration {
                expected: "no host-supplied MCP servers: ACP passthrough is not implemented",
                received: format!("{} host-supplied MCP server(s)", request.mcp_servers.len()),
            });
        }
            });
        }

        let spec = AcpSpec::ChildPipes(StdioSpec::new(
            self.profile.resolved_argv(&request.executable),
        ));
        let launched =
            transport::connect(host, &spec, self.descriptor.vendor_environment_keys).await?;

        let state = Arc::new(SessionState::new(
            request.session_id.clone(),
            host,
            request.configuration.clone(),
        ));
        let connection = Arc::new(
            client::drive(
                launched,
                Arc::clone(&state),
                host.client_info().name.clone(),
            )
            .await?,
        );

        // Every failure from here on ends the child. A `Err` returned with the connection still up
        // would leave an agent running with nothing driving it: the dispatch loop only winds down
        // when the shutdown channel drops, so the process would outlive the call that started it by
        // however long the drop took to reach it.
        let opened = match self.handshake_and_open(&connection, host, &request).await {
            Ok(opened) => opened,
            Err(error) => {
                connection
                    .shutdown(mango_external_agents::CancelReason::Requested)
                    .await;
                return Err(error);
            }
        };
        let (handshake, opened) = opened;
        let configuration = request.configuration.clone();

        let session = AcpSession::new(
            SessionInfo {
                ids: SessionIds {
                    session_id: request.session_id,
                    native_session_id: opened.session_id.to_string(),
                },
                resumed: opened.resumed,
                fallback_reason: opened.fallback_reason,
                effective_configuration: configuration.clone(),
                capabilities: Self::capabilities_from(&handshake.capabilities),
            },
            Arc::clone(&self.profile),
            host.clone(),
            state,
            Arc::clone(&connection),
            opened.session_id,
            handshake.capabilities,
        );

        // Set only when the profile knows the agent's own id for the level *and* the agent
        // advertised it. A mode this agent never offered is a refusal rather than a request it would
        // reject mid-session.
        if let Err(error) = self
            .apply_mode(&session, &configuration, opened.modes.as_ref())
            .await
        {
            // Closing the session rather than the connection alone: a session that exists on the
            // agent's side and is about to be dropped on ours is a session to end, and `close` is
            // what withdraws its pending questions and ends the child.
            let _ = session
                .close(mango_external_agents::CloseReason::Requested)
                .await;
            return Err(error);
        }
        Ok(Box::new(session))
    }
}

/// What the handshake established.
struct Handshake {
    capabilities: AgentCapabilities,
}

/// What opening or resuming produced.
struct Opened {
    session_id: AcpSessionId,
    resumed: bool,
    fallback_reason: Option<String>,
    modes: Option<SessionModeState>,
}

impl AcpHarness {
    /// The handshake and the session, as one fallible step the caller can tear down after.
    async fn handshake_and_open(
        &self,
        connection: &client::ConnectionHandle,
        host: &HostContext,
        request: &OpenSession,
    ) -> Result<(Handshake, Opened)> {
        let handshake = self.initialize(connection, host).await?;
        // The working directory is the host's authorised one and nothing wider: the library has no
        // other directory it is allowed to name to an agent.
        let opened = self
            .open(
                connection,
                host,
                request,
                &handshake,
                host.cwd().to_path_buf(),
            )
            .await?;
        Ok((handshake, opened))
    }

    /// Tells the agent which of its own modes to run in, when the profile knows one.
    async fn apply_mode(
        &self,
        session: &AcpSession,
        configuration: &Configuration,
        modes: Option<&SessionModeState>,
    ) -> Result<()> {
        let Some(mode) = self.mode_for(configuration, modes)? else {
            return Ok(());
        };
        session.set_mode(&mode).await
    }

    async fn initialize(
        &self,
        connection: &client::ConnectionHandle,
        host: &HostContext,
    ) -> Result<Handshake> {
        let client = host.client_info();
        let response = self
            .request(
                connection,
                host,
                "initialize",
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(Self::client_capabilities())
                    // The host's own name, never the library's: an agent reading its logs should see
                    // which product launched it.
                    .client_info(Implementation::new(
                        client.name.clone(),
                        client.version.clone(),
                    )),
            )
            .await?;

        if response.protocol_version != ProtocolVersion::V1 {
            // A version this harness does not speak is refused before a session exists. Negotiating
            // down would mean sending v1 messages to an agent that answered something else.
            return Err(Error::Protocol {
                expected: format!("protocol version {}", ProtocolVersion::V1.as_u16()),
                received: response.protocol_version.as_u16().to_string(),
            });
        }
        Ok(Handshake {
            capabilities: response.agent_capabilities,
        })
    }

    async fn open(
        &self,
        connection: &client::ConnectionHandle,
        host: &HostContext,
        request: &OpenSession,
        handshake: &Handshake,
        cwd: std::path::PathBuf,
    ) -> Result<Opened> {
        let Some(resume) = &request.resume else {
            return self.new_session(connection, host, cwd).await;
        };

        if !handshake.capabilities.load_session {
            if resume.mode == ResumeMode::Strict {
                // Not `NotSupported`, which names an optional `Session` method: resuming is not one,
                // and a host asked for a specific conversation this agent cannot reopen.
                return Err(Error::Protocol {
                    expected: String::from("an agent advertising loadSession"),
                    received: format!("{} with loadSession false", self.profile.id),
                });
            }
            let mut opened = self.new_session(connection, host, cwd).await?;
            opened.fallback_reason =
                Some(String::from("this agent does not advertise session/load"));
            return Ok(opened);
        }

        let loaded = self
            .request(
                connection,
                host,
                "session/load",
                LoadSessionRequest::new(
                    AcpSessionId::new(resume.native_session_id.clone()),
                    cwd.clone(),
                ),
            )
            .await;
        match loaded {
            Ok(loaded) => Ok(Opened {
                session_id: AcpSessionId::new(resume.native_session_id.clone()),
                resumed: true,
                fallback_reason: None,
                modes: loaded.modes,
            }),
            Err(error) if resume.mode == ResumeMode::Strict => Err(error),
            // Fallback: a fresh conversation, and the host is told why rather than left to notice
            // that its history disappeared.
            Err(error) => {
                let mut opened = self.new_session(connection, host, cwd).await?;
                opened.fallback_reason = Some(error.to_string());
                Ok(opened)
            }
        }
    }

    async fn new_session(
        &self,
        connection: &client::ConnectionHandle,
        host: &HostContext,
        cwd: std::path::PathBuf,
    ) -> Result<Opened> {
        // Host-supplied MCP servers were refused before launch; none are attached here.
        let response = self
            .request(connection, host, "session/new", NewSessionRequest::new(cwd))
            .await?;
        Ok(Opened {
            session_id: response.session_id,
            resumed: false,
            fallback_reason: None,
            modes: response.modes,
        })
    }

    /// Sends one handshake request under the host's deadline.
    async fn request<Request>(
        &self,
        connection: &client::ConnectionHandle,
        host: &HostContext,
        method: &'static str,
        request: Request,
    ) -> Result<Request::Response>
    where
        Request: agent_client_protocol::JsonRpcRequest,
        Request::Response: Send,
    {
        client::send(
            connection,
            &self.profile,
            host.limits().request_timeout,
            method,
            request,
        )
        .await
    }

    /// The agent's own mode id for this configuration, when one has to be sent.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the profile names a mode the agent did not advertise: sending it
    /// would be refused mid-session, and the level a host asked for would silently not apply.
    fn mode_for(
        &self,
        configuration: &Configuration,
        modes: Option<&SessionModeState>,
    ) -> Result<Option<String>> {
        let Some(wanted) = configuration
            .level
            .and_then(|level| self.profile.modes.for_level(level))
        else {
            return Ok(None);
        };
        let advertised = modes.is_some_and(|state| {
            state
                .available_modes
                .iter()
                .any(|mode| mode.id.to_string() == wanted)
        });
        if !advertised {
            return Err(Error::Protocol {
                expected: format!("an agent advertising the mode {wanted:?}"),
                received: format!(
                    "{:?}",
                    modes.map(|state| state
                        .available_modes
                        .iter()
                        .map(|mode| mode.id.to_string())
                        .collect::<Vec<_>>())
                ),
            });
        }
        Ok(Some(String::from(wanted)))
    }
}

/// Runs the profile's version argv and returns what it printed.
async fn run_version(
    host: &HostContext,
    descriptor: &HarnessDescriptor,
    argv: Vec<String>,
) -> Result<String> {
    let process = host
        .launcher()
        .spawn(LaunchSpec {
            argv,
            cwd: host.cwd().to_path_buf(),
            env: host.child_environment(descriptor.vendor_environment_keys),
            // A version probe writes nothing: a child with a stdin nobody closes is a child that
            // may wait for input instead of printing a version and exiting.
            stdin: false,
            hide_window: true,
        })
        .await?;

    let mut lines = LineStream::new(process.stdout, host.limits().line);
    let mut printed = String::new();
    // Bounded by the host's own request timeout: an agent that prints nothing and does not exit must
    // not hold a discovery open.
    let read = tokio::time::timeout(host.limits().request_timeout, async {
        while let Some(line) = lines.next_line().await? {
            printed.push_str(&line);
            printed.push('\n');
            if version::parse(&printed).is_some() {
                break;
            }
        }
        Ok::<(), Error>(())
    })
    .await;

    // The child is ended either way: a probe that left one running would leak a process per probe.
    let _ = process
        .control
        .kill(mango_external_agents::CancelReason::Requested)
        .await;
    match read {
        Ok(Ok(())) | Err(_) => Ok(printed),
        Ok(Err(error)) => Err(error),
    }
}

/// Whether a reported version clears the floor a profile pinned.
fn gate(version: Option<&str>, minimum: Option<&str>) -> GateVerdict {
    let Some(version) = version else {
        // It started and printed something unreadable. Not a refusal: an agent that changed the
        // shape of `--version` is not an agent that stopped working, and a host may still try.
        return GateVerdict::Unknown;
    };
    let Some(minimum) = minimum else {
        return GateVerdict::Usable;
    };
    match version::compare(version, minimum) {
        Comparison::AtLeast => GateVerdict::Usable,
        Comparison::Below => GateVerdict::VersionTooOld {
            found: version.to_owned(),
            minimum: minimum.to_owned(),
        },
        Comparison::Unknown => GateVerdict::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{AcpHarness, ceiling, gate};
    use mango_external_agents::{Capabilities, GateVerdict, Harness, HarnessKind};

    #[test]
    fn a_harness_is_named_by_its_profile() {
        let harness = AcpHarness::builtin("cursor").expect("expected the cursor profile");
        assert_eq!(
            harness.descriptor().kind,
            HarnessKind::Acp(mango_external_agents::AcpProfileId::new("cursor"))
        );
        assert_eq!(harness.descriptor().transports.len(), 1);
    }

    #[test]
    fn one_harness_per_builtin_profile_is_ready_to_register() {
        let harnesses = AcpHarness::builtins();
        assert_eq!(harnesses.len(), crate::profile::builtin_profiles().len());
        mango_external_agents::HarnessRegistry::new(harnesses)
            .expect("expected distinct kinds, received a duplicate");
    }

    /// The four ACP v1 has no surface for. A ceiling that claimed them would make every discovery a
    /// promise the dialect cannot keep.
    #[test]
    fn the_ceiling_claims_nothing_acp_v1_has_no_surface_for() {
        let ceiling = ceiling();
        assert!(!ceiling.steering, "session/prompt is one request per turn");
        assert!(!ceiling.model_catalog, "session/new takes no model");
        assert!(!ceiling.native_review);
        assert!(!ceiling.account_usage);
        assert!(
            !ceiling.mcp_passthrough,
            "nothing on OpenSession carries MCP servers to pass through"
        );
        assert!(ceiling.within(&Capabilities::all()));
    }

    /// Nobody has pinned a floor for any built-in profile, so a version that cannot be read must not
    /// gate an agent that might work.
    #[test]
    fn an_unreadable_version_is_unknown_rather_than_a_refusal() {
        assert_eq!(gate(None, None), GateVerdict::Unknown);
        assert_eq!(gate(None, Some("1.0.0")), GateVerdict::Unknown);
        assert_eq!(gate(Some("nightly"), Some("1.0.0")), GateVerdict::Unknown);
    }

    #[test]
    fn a_version_is_gated_only_against_a_floor_the_profile_pinned() {
        assert_eq!(gate(Some("0.1.0"), None), GateVerdict::Usable);
        assert_eq!(gate(Some("2.0.0"), Some("1.0.0")), GateVerdict::Usable);
        assert_eq!(
            gate(Some("0.9.0"), Some("1.0.0")),
            GateVerdict::VersionTooOld {
                found: String::from("0.9.0"),
                minimum: String::from("1.0.0"),
            }
        );
    }

    /// The host owns files and terminals. An agent that could ask this client to write one would be
    /// asking the library to act on the host's filesystem on a third party's instruction.
    #[test]
    fn the_client_declines_every_filesystem_and_terminal_capability() {
        let capabilities = AcpHarness::client_capabilities();
        assert!(!capabilities.fs.read_text_file);
        assert!(!capabilities.fs.write_text_file);
        assert!(!capabilities.terminal);
    }
}
