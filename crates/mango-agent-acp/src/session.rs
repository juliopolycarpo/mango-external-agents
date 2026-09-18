//! One live ACP conversation.
//!
//! The shape of a turn on this dialect: `session/prompt` is a *request*, and its response is the
//! turn's end. Everything the host will see arrives in the meantime as `session/update`
//! notifications, so the prompt cannot be awaited inline — [`AcpSession::start_turn`] sends it, hands
//! the stream back, and a task waits for the response and ends the turn with it.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/prompt-turn>

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, ListSessionsRequest, PromptRequest,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionId as AcpSessionId, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
};
use mango_external_agents::configuration::{
    Configuration, ConfigurationCatalog, ConfigurationCategory, ConfigurationChange,
    ConfigurationOption, ConfigurationOptionId, ConfigurationOptionValue, ConfigurationOutcome,
    ConfigurationPatch, ConfigurationState, ConfigurationValue, ConfigurationValueType,
    RejectedSetting, Rollback, SettingRejection,
};
use mango_external_agents::event::EventKind;
use mango_external_agents::session::{
    AccountUsage, CancelReason, CloseReason, NativeSession, Session, SessionIds, SessionPage,
    SessionQuery, TurnRequest,
};
use mango_external_agents::state::SessionStatus;
use mango_external_agents::{
    Capability, Dispatch, Error, EventSink, HostContext, PermissionResponse, Result,
    SessionLifecycle, TurnStream,
};

use crate::client::{self, ConnectionHandle, link_failure, with_stderr};
use crate::profile::AcpProfile;
use crate::{content, reducer};

/// One conversation with one ACP agent.
pub struct AcpSession {
    profile: Arc<AcpProfile>,
    host: HostContext,
    /// The dispatch loop's own shared state: the running turn, pending approvals, the inherited
    /// configuration and a handle to `session_state` for publishing session facts as they arrive.
    connection_state: Arc<client::SessionState>,
    /// The core's live, observable session state — the one thing [`Session::state`] hands back.
    session_state: mango_external_agents::SessionState,
    connection: Arc<ConnectionHandle>,
    native_session_id: AcpSessionId,
    agent_capabilities: AgentCapabilities,
    modes: Option<agent_client_protocol::schema::v1::SessionModeState>,
    configuration_gate: tokio::sync::Mutex<()>,
    lifecycle: Arc<SessionLifecycle>,
    /// The close owner records one result independently of the `close` future that started it.
    close: Arc<CloseState>,
}

/// Shared completion for the one close task a session admits.
pub(crate) struct CloseState {
    started: AtomicBool,
    done: mango_external_agents::CancelToken,
    result: Mutex<Option<std::result::Result<(), String>>>,
}

impl Default for CloseState {
    fn default() -> Self {
        Self {
            started: AtomicBool::new(false),
            done: mango_external_agents::CancelToken::new(),
            result: Mutex::new(None),
        }
    }
}

impl CloseState {
    fn claim(&self) -> bool {
        !self.started.swap(true, Ordering::AcqRel)
    }

    /// Whether an explicit session close already owns teardown.
    pub(crate) fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    fn finish(&self, result: Result<()>) {
        *self.result.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(result.map_err(|error| error.to_string()));
        self.done.cancel();
    }

    async fn wait(&self) -> Result<()> {
        self.done.cancelled().await;
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Err(String::from("the ACP close task ended without an outcome")))
            .map_err(|message| {
                Error::Vendor(link_failure(format!(
                    "ACP session cleanup failed: {message}"
                )))
            })
    }
}

/// State accumulated while ACP applies a non-atomic configuration patch.
struct ConfigurationProgress {
    requested: Configuration,
    accepted: Configuration,
    catalog: ConfigurationCatalog,
    applied: Vec<ConfigurationOptionId>,
    rejected: Vec<RejectedSetting>,
}

impl std::fmt::Debug for AcpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSession")
            .field("profile", &self.profile.id)
            .field("ids", &self.session_state.snapshot().ids)
            .field("closed", &self.lifecycle.is_closed())
            .finish_non_exhaustive()
    }
}

impl AcpSession {
    #[allow(
        clippy::too_many_arguments,
        reason = "one session, seven facts about it"
    )]
    pub(crate) fn new(
        profile: Arc<AcpProfile>,
        host: HostContext,
        connection_state: Arc<client::SessionState>,
        session_state: mango_external_agents::SessionState,
        connection: Arc<ConnectionHandle>,
        native_session_id: AcpSessionId,
        agent_capabilities: AgentCapabilities,
        modes: Option<agent_client_protocol::schema::v1::SessionModeState>,
    ) -> Self {
        Self {
            profile,
            host,
            connection_state,
            session_state,
            connection,
            native_session_id,
            agent_capabilities,
            lifecycle: Arc::new(SessionLifecycle::default()),
            close: Arc::new(CloseState::default()),
            modes,
            configuration_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Tells the agent which of its own modes to run in.
    ///
    /// # Errors
    ///
    /// Whatever the agent answered.
    pub(crate) async fn set_mode(&self, mode_id: &str) -> Result<()> {
        self.request(
            "session/set_mode",
            SetSessionModeRequest::new(self.native_session_id.clone(), String::from(mode_id)),
        )
        .await
        .map(|_| ())
    }

    /// Applies one complete live configuration patch while no prompt is in flight.
    ///
    /// ACP v1's `session/set_config_option` returns the whole option list after each request. The
    /// response, rather than the request, is what updates the public catalog and observed state.
    pub(crate) async fn configure_session(
        &self,
        patch: ConfigurationPatch,
    ) -> Result<ConfigurationOutcome> {
        let _configuration = self.configuration_gate.lock().await;
        if self.connection_state.turn().is_some() {
            return Err(Error::Protocol {
                expected: String::from(
                    "a configuration change between ACP session/prompt requests",
                ),
                received: String::from("an active session/prompt request"),
            });
        }

        let mut rejected = reset_rejections(&patch);
        let mut applied = Vec::new();
        let mut accepted = self.connection_state.configuration();
        let mut catalog = self.session_state.snapshot().catalog.clone();
        let mut requested = self
            .session_state
            .snapshot()
            .configuration
            .requested
            .clone();
        requested = requested.patched(&patch);

        for (axis, change) in [("model", &patch.model), ("effort", &patch.effort)] {
            let ConfigurationChange::Set(value) = change else {
                continue;
            };
            let category = match axis {
                "model" => ConfigurationCategory::Model,
                "effort" => ConfigurationCategory::ReasoningEffort,
                _ => unreachable!("only the two declared axes are iterated"),
            };
            let Some(option) = unique_option_in(&catalog, &category) else {
                rejected.push(RejectedSetting::new(
                    ConfigurationOptionId::new(axis),
                    SettingRejection::UnknownOption,
                ));
                continue;
            };
            let id = option.id.clone();
            let chosen = value.clone();
            let value = ConfigurationValue::Text(value.clone());
            if let Some(rejection) = validate_value(option, &value) {
                rejected.push(RejectedSetting::new(id, rejection));
                continue;
            }
            catalog = match self.set_config_option(&id, value.clone()).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    return self.option_failure_outcome(
                        id,
                        error,
                        ConfigurationProgress {
                            requested,
                            accepted,
                            catalog,
                            applied,
                            rejected,
                        },
                    );
                }
            };
            accepted = match axis {
                "model" => accepted.with_model(chosen),
                "effort" => accepted.with_effort(chosen),
                _ => unreachable!("only the two declared axes are iterated"),
            };
            applied.push(id);
        }

        for (id, change) in &patch.native {
            let ConfigurationChange::Set(value) = change else {
                continue;
            };
            let Some(option) = catalog.option(id) else {
                rejected.push(RejectedSetting::new(
                    id.clone(),
                    SettingRejection::UnknownOption,
                ));
                continue;
            };
            if let Some(rejection) = validate_value(option, value) {
                rejected.push(RejectedSetting::new(id.clone(), rejection));
                continue;
            }
            catalog = match self.set_config_option(id, value.clone()).await {
                Ok(catalog) => catalog,
                Err(error) => {
                    return self.option_failure_outcome(
                        id.clone(),
                        error,
                        ConfigurationProgress {
                            requested,
                            accepted,
                            catalog,
                            applied,
                            rejected,
                        },
                    );
                }
            };
            accepted = accepted.with_native(id.clone(), value.clone());
            applied.push(id.clone());
        }

        // Modes predate config options and are still supported by ACP v1. They remain separate
        // from the catalog because a profile, not a category label, establishes their permission
        // meaning.
        if let ConfigurationChange::Set(level) = patch.level {
            let routing = match patch.routing {
                ConfigurationChange::Set(routing) => routing,
                _ => accepted
                    .routing
                    .unwrap_or(mango_external_agents::ApprovalRouting::User),
            };
            if !crate::profile::matrix(&self.profile.modes).supports(level, routing) {
                rejected.push(RejectedSetting::new(
                    ConfigurationOptionId::new("level"),
                    SettingRejection::RefusedByVendor {
                        detail: String::from(
                            "the ACP profile cannot establish that permission level",
                        ),
                    },
                ));
            } else if let Some(mode) = self.profile.modes.for_level(level) {
                let advertised = self.modes.as_ref().is_some_and(|modes| {
                    modes
                        .available_modes
                        .iter()
                        .any(|available| available.id.to_string() == mode)
                });
                if !advertised {
                    rejected.push(RejectedSetting::new(
                        ConfigurationOptionId::new("level"),
                        SettingRejection::RefusedByVendor {
                            detail: String::from(
                                "the ACP agent did not advertise the configured mode",
                            ),
                        },
                    ));
                } else {
                    if let Err(error) = self.set_mode(mode).await {
                        return self.option_failure_outcome(
                            ConfigurationOptionId::new("level"),
                            error,
                            ConfigurationProgress {
                                requested,
                                accepted,
                                catalog,
                                applied,
                                rejected,
                            },
                        );
                    }
                    accepted = accepted.with_level(level);
                    applied.push(ConfigurationOptionId::new("level"));
                }
            } else {
                accepted = accepted.with_level(level);
                applied.push(ConfigurationOptionId::new("level"));
            }
        }
        if let ConfigurationChange::Set(routing) = patch.routing {
            let level = match patch.level {
                ConfigurationChange::Set(level) => Some(level),
                _ => accepted.level,
            };
            if level.is_some_and(|level| {
                !crate::profile::matrix(&self.profile.modes).supports(level, routing)
            }) {
                rejected.push(RejectedSetting::new(
                    ConfigurationOptionId::new("routing"),
                    SettingRejection::RefusedByVendor {
                        detail: String::from(
                            "the ACP profile cannot establish that permission routing",
                        ),
                    },
                ));
            } else {
                accepted = accepted.with_routing(routing);
                applied.push(ConfigurationOptionId::new("routing"));
            }
        }

        let state = self.publish_catalog_configuration(&requested, &accepted, catalog);
        let outcome = ConfigurationOutcome::applied(state, applied);
        if rejected.is_empty() {
            Ok(outcome)
        } else {
            Ok(outcome.rejecting(rejected, Rollback::NotAttempted))
        }
    }

    async fn set_config_option(
        &self,
        id: &ConfigurationOptionId,
        value: ConfigurationValue,
    ) -> Result<ConfigurationCatalog> {
        let value = match value {
            ConfigurationValue::Text(value) => SessionConfigOptionValue::from(value.as_str()),
            ConfigurationValue::Boolean(value) => SessionConfigOptionValue::from(value),
            ConfigurationValue::Integer(value) => {
                return Err(Error::Protocol {
                    expected: String::from("an ACP v1 select id or boolean configuration value"),
                    received: value.to_string(),
                });
            }
            _ => {
                return Err(Error::Protocol {
                    expected: String::from("an ACP v1 select id or boolean configuration value"),
                    received: String::from("an unknown configuration value type"),
                });
            }
        };
        let response = self
            .request(
                "session/set_config_option",
                SetSessionConfigOptionRequest::new(
                    self.native_session_id.clone(),
                    String::from(id.as_str()),
                    value,
                ),
            )
            .await?;
        Ok(catalog_from_options(&response.config_options))
    }

    /// Publishes the configuration state confirmed before a later option request can fail.
    fn publish_catalog_configuration(
        &self,
        requested: &Configuration,
        accepted: &Configuration,
        catalog: ConfigurationCatalog,
    ) -> ConfigurationState {
        self.connection_state.accept_configuration(accepted.clone());
        let state = ConfigurationState::new(
            requested.clone(),
            accepted.clone(),
            configuration_from_catalog(&catalog),
        );
        self.session_state.update(|snapshot| {
            snapshot.catalog = catalog;
            snapshot.configuration = state.clone();
        });
        state
    }

    /// Publishes every setting confirmed before a vendor explicitly refuses a later option.
    fn option_failure_outcome(
        &self,
        option: ConfigurationOptionId,
        error: Error,
        mut progress: ConfigurationProgress,
    ) -> Result<ConfigurationOutcome> {
        let state = self.publish_catalog_configuration(
            &progress.requested,
            &progress.accepted,
            progress.catalog,
        );
        if !matches!(error.cause(), Error::Vendor(_)) {
            return Err(error);
        }
        progress.rejected.push(RejectedSetting::new(
            option,
            SettingRejection::RefusedByVendor {
                detail: String::from("the ACP agent refused this configuration option"),
            },
        ));
        // ACP v1 has no reset operation for a configuration option, so undoing an earlier accepted
        // set would only create another unverified state. Keep the confirmed partial state instead.
        Ok(ConfigurationOutcome::applied(state, progress.applied)
            .rejecting(progress.rejected, Rollback::NotAttempted))
    }

    /// Sends one request under the host's deadline.
    ///
    /// Everything but `session/prompt`, which is a turn and stays unbounded.
    async fn request<Request>(
        &self,
        method: &'static str,
        request: Request,
    ) -> Result<Request::Response>
    where
        Request: agent_client_protocol::JsonRpcRequest,
        Request::Response: Send,
    {
        client::send(
            &self.connection,
            &self.profile,
            self.host.limits().request_timeout,
            method,
            request,
        )
        .await
    }

    /// The configuration one turn runs under, refusing what ACP v1 has no surface for.
    ///
    /// A per-turn patch goes through exactly the checks `open_session` applies, and for the same
    /// reason: a pair this profile cannot run has to be refused rather than quietly replaced by one
    /// it can. The case that made this a real hole was a turn asking for
    /// [`PermissionLevel::FullAccess`](mango_external_agents::PermissionLevel) on a profile with no
    /// mode for it — nothing would have set a mode, nothing would have refused a request, and the
    /// turn would have run as `Default` while the host believed it had granted more.
    fn effective(&self, request: &TurnRequest) -> Result<Configuration> {
        let base = self.connection_state.configuration();
        let configuration = match &request.configuration {
            Some(patch) => {
                refuse_unsupported_reset(patch)?;
                base.patched(patch)
            }
            None => base,
        };
        // ACP configuration options are session-scoped. A turn cannot set them and report an
        // accepted inherited value before `session/set_config_option` has run between turns.
        if let Some(patch) = request.configuration.as_ref().filter(|patch| {
            !patch.native.is_empty() || !patch.model.is_keep() || !patch.effort.is_keep()
        }) {
            let received = patch.native.keys().next().map_or_else(
                || {
                    if !patch.model.is_keep() {
                        String::from("per-turn ACP model configuration")
                    } else {
                        String::from("per-turn ACP reasoning-effort configuration")
                    }
                },
                |id| format!("per-turn ACP configuration patch for {}", id.as_str()),
            );
            return Err(Error::Protocol {
                expected: String::from(
                    "a configuration change through Session::configure between ACP turns",
                ),
                received,
            });
        }
        if let Some(level) = configuration.level {
            let routing = configuration
                .routing
                .unwrap_or(mango_external_agents::ApprovalRouting::User);
            if !crate::profile::matrix(&self.profile.modes).supports(level, routing) {
                return Err(Error::HostConfiguration {
                    expected: "a (level, routing) pair this profile supports",
                    // As in `AcpHarness::open_session`: the pair is the library's own vocabulary,
                    // the profile id is the host's.
                    received: format!("{level:?}/{routing:?}"),
                });
            }
        }
        // Compared by *mode*, not by level. A level the profile reaches through a mode would need a
        // `session/set_mode` mid-session, and this harness does not change a session's mode under a
        // running conversation — so any pair whose mode differs from the one the session was opened
        // with is refused, in both directions. Narrowing looks harmless and is not: a turn asking for
        // `ReadOnly` on a session the agent runs in its own full-access mode would register a
        // read-only handle while the agent, still in that mode, raises no permission request at all
        // for the standing refusal to answer. Nothing would hold the level the host asked for.
        // ACP modes are selected once, while the session opens. The live inherited configuration
        // may later gain mode-free overrides, but it must not become the baseline used to infer a
        // mode this running agent never received.
        let session_level = self.session_state.snapshot().configuration.accepted.level;
        let wanted_mode = configuration
            .level
            .and_then(|level| self.profile.modes.for_level(level));
        let session_mode = session_level.and_then(|level| self.profile.modes.for_level(level));
        if configuration.level != session_level && wanted_mode != session_mode {
            return Err(Error::Protocol {
                // As in `AcpHarness::mode_for`: the relationship, never the mode id.
                expected: String::from(
                    "a turn whose level runs under the mode this profile maps the session's own level to: ACP modes are set when the session opens",
                ),
                received: format!(
                    "{:?}, which wants mode {wanted_mode:?}",
                    configuration.level
                ),
            });
        }
        Ok(configuration)
    }

    /// Publishes what this harness actually applied, next to what was asked.
    ///
    /// `accepted` carries only settings that the session actually encoded.
    fn publish_configuration(&self, configuration: &Configuration) {
        let state = self.session_state.snapshot().configuration.clone();
        self.session_state.set_configuration(
            state
                .with_requested(configuration.clone())
                .with_accepted(configuration.clone()),
        );
    }
}

/// The subset of a [`Configuration`] established outside the config-option service.
pub(crate) fn accepted_axes(configuration: &Configuration) -> Configuration {
    let mut accepted = Configuration::unknown();
    if let Some(level) = configuration.level {
        accepted = accepted.with_level(level);
    }
    if let Some(routing) = configuration.routing {
        accepted = accepted.with_routing(routing);
    }
    accepted
}

/// Lists only conversations in the workspace the current host authorized.
pub(crate) async fn list_sessions(
    connection: &ConnectionHandle,
    profile: &AcpProfile,
    host: &HostContext,
    capabilities: &AgentCapabilities,
    query: SessionQuery,
) -> Result<SessionPage> {
    if capabilities.session_capabilities.list.is_none() {
        return Err(Error::not_supported(Capability::SessionListing));
    }
    validate_listing_workspace(host, &query)?;
    let mut request = ListSessionsRequest::new().cwd(host.cwd().to_path_buf());
    if let Some(cursor) = query.cursor {
        request = request.cursor(cursor);
    }
    let response = client::send(
        connection,
        profile,
        host.limits().request_timeout,
        "session/list",
        request,
    )
    .await?;
    let workspace = host.cwd().display().to_string();
    Ok(SessionPage {
        sessions: response
            .sessions
            .into_iter()
            .filter(|session| session.cwd == host.cwd())
            .map(|session| NativeSession {
                native_session_id: session.session_id.to_string(),
                title: session.title,
                preview: None,
                workspace_path: Some(workspace.clone()),
                updated_at: session
                    .updated_at
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
                    .map(std::time::SystemTime::from),
            })
            .collect(),
        next_cursor: response.next_cursor,
        truncated: false,
    })
}

/// Refuses a listing request that would widen the workspace the host authorized.
pub(crate) fn validate_listing_workspace(host: &HostContext, query: &SessionQuery) -> Result<()> {
    if query
        .workspace_path
        .as_ref()
        .is_none_or(|path| path == host.cwd())
    {
        return Ok(());
    }
    Err(Error::HostConfiguration {
        expected: "the host-authorized workspace for ACP session listing",
        received: String::from("a different workspace path"),
    })
}

/// Refuses a patch that asks to remove an override this harness has no way to remove.
///
/// A thin, named wrapper over [`mango_external_agents::configuration::refuse_unsupported_reset`]:
/// every axis this harness accepts — level, through a `session/set_mode` chosen once at open, and
/// routing, decided locally — has no vendor-side "put it back" either, so a reset request is refused
/// the same way everywhere it can be asked, rather than only where it happens to be checked.
pub(crate) fn refuse_unsupported_reset(patch: &ConfigurationPatch) -> Result<()> {
    mango_external_agents::configuration::refuse_unsupported_reset(patch)
}

/// Marks one owned prompt as cancelled and asks the agent to stop it.
///
/// The start gate keeps this notification behind the prompt it belongs to. A stream-drop or
/// transcript-overflow path may call it before a host calls `Session::cancel`; only the first caller
/// records a reason and writes the notification.
fn request_native_cancel(
    state: &client::SessionState,
    connection: &agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    session_id: AcpSessionId,
    reason: CancelReason,
) -> bool {
    let _starting = state.lock_turn_start();
    let Some(handle) = state.turn() else {
        return true;
    };
    if !state.begin_cancellation(reason) {
        return true;
    }
    if connection
        .send_notification(CancelNotification::new(session_id))
        .is_ok()
    {
        return true;
    }
    state.record_cancel_failure(&handle, "ACP session/cancel could not be queued");
    false
}

/// Converts ACP's complete live option list into the core's catalog vocabulary.
pub(crate) fn catalog_from_options(options: &[SessionConfigOption]) -> ConfigurationCatalog {
    ConfigurationCatalog::new(options.iter().cloned().map(configuration_option).collect())
        .normalized()
}

/// Converts one protocol option while retaining its agent-defined order and current value.
fn configuration_option(option: SessionConfigOption) -> ConfigurationOption {
    let id = ConfigurationOptionId::new(option.id.to_string());
    let category = match option.category {
        Some(SessionConfigOptionCategory::Model) => ConfigurationCategory::Model,
        Some(SessionConfigOptionCategory::ThoughtLevel) => ConfigurationCategory::ReasoningEffort,
        Some(SessionConfigOptionCategory::Mode) => ConfigurationCategory::Mode,
        Some(SessionConfigOptionCategory::ModelConfig) => {
            ConfigurationCategory::Other(String::from("model_config"))
        }
        Some(SessionConfigOptionCategory::Other(value)) => ConfigurationCategory::Other(value),
        None => ConfigurationCategory::Other(String::from("uncategorized")),
        _ => ConfigurationCategory::Other(String::from("unknown")),
    };
    let mut mapped = match option.kind {
        SessionConfigKind::Select(select) => {
            let current = ConfigurationValue::Text(select.current_value.to_string());
            let values = select_values(select.options)
                .into_iter()
                .map(|value| {
                    ConfigurationOptionValue::new(ConfigurationValue::Text(value.value.to_string()))
                        .with_display_name(value.name)
                        .with_description(value.description.unwrap_or_default())
                })
                .collect();
            ConfigurationOption::new(id, category, ConfigurationValueType::Enumerated)
                .with_name(option.name)
                .with_current(current)
                .with_values(values)
        }
        SessionConfigKind::Boolean(boolean) => {
            ConfigurationOption::new(id, category, ConfigurationValueType::Boolean)
                .with_name(option.name)
                .with_current(ConfigurationValue::Boolean(boolean.current_value))
        }
        _ => ConfigurationOption::new(
            id,
            category,
            ConfigurationValueType::Other(String::from("unknown")),
        )
        .with_name(option.name),
    };
    if let Some(description) = option.description {
        mapped = mapped.with_description(description);
    }
    mapped
}

/// Flattens grouped selectors without changing the order within each group.
fn select_values(
    options: SessionConfigSelectOptions,
) -> Vec<agent_client_protocol::schema::v1::SessionConfigSelectOption> {
    match options {
        SessionConfigSelectOptions::Ungrouped(values) => values,
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.into_iter().flat_map(|group| group.options).collect()
        }
        _ => Vec::new(),
    }
}

/// The values the agent currently reports under recognised semantic categories.
pub(crate) fn configuration_from_catalog(catalog: &ConfigurationCatalog) -> Configuration {
    let mut configuration = Configuration::unknown();
    if let Some(option) = unique_option_in(catalog, &ConfigurationCategory::Model)
        && let Some(ConfigurationValue::Text(value)) = &option.current
    {
        configuration = configuration.with_model(value.clone());
    }
    if let Some(option) = unique_option_in(catalog, &ConfigurationCategory::ReasoningEffort)
        && let Some(ConfigurationValue::Text(value)) = &option.current
    {
        configuration = configuration.with_effort(value.clone());
    }
    for option in catalog.options() {
        if matches!(
            option.category,
            ConfigurationCategory::Model | ConfigurationCategory::ReasoningEffort
        ) {
            continue;
        }
        if let Some(value) = &option.current {
            configuration = configuration.with_native(option.id.clone(), value.clone());
        }
    }
    configuration
}

/// Finds an unambiguous mapped option. ACP allows repeats, so a category alone cannot choose one.
fn unique_option_in<'a>(
    catalog: &'a ConfigurationCatalog,
    category: &ConfigurationCategory,
) -> Option<&'a ConfigurationOption> {
    let mut matches = catalog
        .options()
        .iter()
        .filter(|option| &option.category == category);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Validates the scalar form and select membership before any request reaches an agent.
fn validate_value(
    option: &ConfigurationOption,
    value: &ConfigurationValue,
) -> Option<SettingRejection> {
    let matches_kind = matches!(
        (&option.value_type, value),
        (
            ConfigurationValueType::Enumerated,
            ConfigurationValue::Text(_)
        ) | (
            ConfigurationValueType::Boolean,
            ConfigurationValue::Boolean(_)
        )
    );
    if !matches_kind {
        return Some(SettingRejection::UnsupportedValue {
            received: format!("{:?}", value.value_type()),
        });
    }
    if option.value_type == ConfigurationValueType::Enumerated
        && !option
            .values
            .iter()
            .any(|candidate| candidate.value == *value)
    {
        return Some(SettingRejection::UnsupportedValue {
            received: match value {
                ConfigurationValue::Text(value) => value.clone(),
                _ => String::from("a non-text select value"),
            },
        });
    }
    None
}

/// Every reset is a refusal: ACP v1 only sets explicit current values.
fn reset_rejections(patch: &ConfigurationPatch) -> Vec<RejectedSetting> {
    let mut rejections = Vec::new();
    for (id, reset) in [
        (ConfigurationOptionId::new("model"), patch.model.is_reset()),
        (
            ConfigurationOptionId::new("effort"),
            patch.effort.is_reset(),
        ),
        (ConfigurationOptionId::new("level"), patch.level.is_reset()),
        (
            ConfigurationOptionId::new("routing"),
            patch.routing.is_reset(),
        ),
    ] {
        if reset {
            rejections.push(RejectedSetting::new(
                id,
                SettingRejection::ResetNotSupported,
            ));
        }
    }
    rejections.extend(
        patch
            .native
            .iter()
            .filter(|(_, change)| change.is_reset())
            .map(|(id, _)| RejectedSetting::new(id.clone(), SettingRejection::ResetNotSupported)),
    );
    rejections
}

#[async_trait::async_trait]
impl Session for AcpSession {
    fn state(&self) -> &mango_external_agents::SessionState {
        &self.session_state
    }

    async fn configure(&self, patch: ConfigurationPatch) -> Result<ConfigurationOutcome> {
        self.configure_session(patch).await
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        if self.host.cancel().is_cancelled() {
            return Err(Error::Cancelled {
                reason: CancelReason::Shutdown,
            }
            .with_dispatch(Dispatch::NotSubmitted));
        }
        if self.lifecycle.is_closed() {
            return Err(Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted));
        }
        self.validate_turn_request(&request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let prompt = content::prompt(
            &request.input,
            &request.attachments,
            &self.agent_capabilities.prompt_capabilities,
        )
        .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let _configuration = self.configuration_gate.lock().await;

        let (sink, events) = EventSink::with_limits(
            self.session_state.snapshot().ids.session_id.clone(),
            request.turn_id.clone(),
            request.attempt,
            Arc::clone(self.host.clock()),
            self.host.limits(),
        );
        // A configuration update and the single ACP prompt slot are one transaction. A second
        // caller must not merge from an old snapshot while this one accepts its settings, then win
        // the slot later and restore the stale snapshot over the newer restriction.
        //
        // ACP's `session/prompt` names no per-turn handle of its own, so `native_turn_id` is this
        // harness's own per-session turn sequence number, minted here — before the wire request is
        // sent — rather than read off the JSON-RPC id `send_request` would assign. That id is only
        // known once the request is actually dispatched, and dispatching it has to stay gated behind
        // the same re-entrant start guard `retry_cancel_error` depends on below; minting our own
        // avoids reordering that gate around a value the wire cannot supply early enough. One real
        // cost: unlike Claude's or Codex's native turn ids, this one never appears in a captured
        // JSON-RPC transcript, so a host correlating captures by turn id will not find it there.
        let (handle, native_turn_id, configuration) = {
            let Some(_lifecycle) = self.lifecycle.begin_start() else {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            };
            let _starting = self.connection_state.lock_turn_start();
            if self.host.cancel().is_cancelled() {
                return Err(Error::Cancelled {
                    reason: CancelReason::Shutdown,
                }
                .with_dispatch(Dispatch::NotSubmitted));
            }
            let configuration = self
                .effective(&request)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
            let handle = self
                .connection_state
                .begin_turn(sink.clone(), configuration.level)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
            let native_turn_id = format!("acp-turn-{}", handle.generation);
            (handle, native_turn_id, configuration)
        };

        // Emitted before the prompt is sent, so the first event a host reads names the turn the
        // rest of the stream belongs to.
        if let Err(error) = sink
            .emit(EventKind::TurnStarted {
                native_turn_id: native_turn_id.clone(),
            })
            .await
        {
            // Matched, and claimed: the same release every other turn-end performs. An unconditional
            // take would leave this handle's `finished` flag clear for a turn that is over, and would
            // skip the cancel reason and the questions `end_turn_matching` settles with the slot.
            //
            // Like the teardown guards in `client.rs`, this arm has no regression test: an emit here
            // fails only if the receiving half of the stream is already gone, and the stream is built
            // in this function and handed to the caller after it returns, so nothing can have dropped
            // it yet. It is the correct release for a path that exists as defence in depth.
            if let Some((turn, _, _)) = self.connection_state.prepare_terminal_matching(&handle) {
                let _ = turn.finish();
                self.connection_state.release_turn_matching(&handle);
            }
            if self.lifecycle.is_closed() {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            }
            return Err(error.with_dispatch(Dispatch::NotSubmitted));
        }

        // `TurnStarted` is emitted first, which can give `cancel` a window before ACP has a prompt
        // to cancel. Re-enter the start guard while sending the wire request; if cancellation
        // already won that window, queue a second notification after the prompt so the agent applies
        // it to this turn rather than treating the earlier notification as a no-op.
        let (sent, retry_cancel_error) = {
            let Some(_lifecycle) = self.lifecycle.begin_start() else {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            };
            let _starting = self.connection_state.lock_turn_start();
            if !self.connection_state.can_submit_prompt(&handle) {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            }
            let sent = self
                .connection
                .connection()
                .send_request(PromptRequest::new(self.native_session_id.clone(), prompt));
            // The reserved handle alone is not submission. A close can win while TurnStarted is
            // being published; only inherit this patch once the prompt has entered the SDK.
            self.connection_state
                .accept_configuration(configuration.clone());
            self.publish_configuration(&configuration);
            let retry_cancel_error = match self.connection_state.is_cancelling() {
                true => self
                    .connection
                    .connection()
                    .send_notification(CancelNotification::new(self.native_session_id.clone()))
                    .err()
                    .map(|_| Error::Link {
                        peer: format!("ACP agent {}", self.profile.id),
                        // The library's own summary: `Error::Link`'s is written verbatim, and the
                        // agent's message and its stderr tail are both the agent's words. The
                        // turn's own failure event still carries them.
                        message: String::from("a link that could not carry session/cancel"),
                    }),
                false => None,
            };
            (sent, retry_cancel_error)
        };

        let stream = TurnStream::accepted(
            request.turn_id,
            request.attempt,
            native_turn_id.clone(),
            events,
        )
        // ACP has no prompt acknowledgement or native turn handle. `send_request` only hands the
        // request to the SDK's outgoing queue, so a lost pipe remains ambiguous to the host.
        .with_dispatch(Dispatch::AcceptanceUnknown);
        if retry_cancel_error.is_some() {
            // The prompt is already on the wire. Preserve its owned stream rather than returning an
            // error that discards the only place its terminal can be reported, then stop the peer
            // before releasing this generation for another ACP prompt.
            //
            // Detached before the first await rather than run inline: this function's future
            // belongs to the caller, and a caller that drops it at `wait_shutdown` would take the
            // terminal commit and the slot release down with it. The shutdown itself proceeds in
            // its own task either way, so what such a drop actually left behind was an installed
            // turn on a live session — every later prompt refused as `Busy`, on a stream whose
            // terminal nobody would ever write.
            self.connection.begin_shutdown(CancelReason::Shutdown);
            detach_retry_cancel_cleanup(
                Arc::clone(&self.connection),
                Arc::clone(&self.connection_state),
                handle,
            );
            return Ok(stream.with_dispatch(Dispatch::AcceptanceUnknown));
        }

        let state = Arc::clone(&self.connection_state);
        let connection = Arc::clone(&self.connection);
        let lifecycle = Arc::clone(&self.lifecycle);
        let outgoing = self.connection.connection().clone();
        let control = Arc::clone(self.connection.control());
        let driver_done = self.connection.driver_done().clone();
        let limits = *self.host.limits();
        let native_session_id = self.native_session_id.clone();
        let profile = Arc::clone(&self.profile);
        // Spawned rather than awaited: the response *is* the turn's end, and a host that could not
        // read its events until then would receive the whole turn at once or not at all.
        // `block_task` is sound here for the same reason — this task is not the dispatch loop.
        tokio::spawn(async move {
            let mut prompt = Box::pin(sent.block_task());
            let outcome = tokio::select! {
                outcome = &mut prompt => Some(outcome),
                () = handle.sink.closed() => {
                    if request_native_cancel(&state, &outgoing, native_session_id.clone(), CancelReason::Requested) {
                        tokio::time::timeout(limits.kill_grace, &mut prompt).await.ok()
                    } else {
                        None
                    }
                },
                () = handle.sink.terminated() => {
                    if request_native_cancel(&state, &outgoing, native_session_id.clone(), CancelReason::Requested) {
                        tokio::time::timeout(limits.kill_grace, &mut prompt).await.ok()
                    } else {
                        None
                    }
                },
                () = handle.cancellation.cancelled() => {
                    // `cancel` records a failed notification while it holds this gate. Wait until
                    // that synchronous write has settled before deciding the stream's outcome.
                    let _starting = state.lock_turn_start();
                    drop(_starting);
                    tokio::time::timeout(limits.kill_grace, &mut prompt).await.ok()
                },
                () = driver_done.cancelled() => None,
            };
            let cleanup_error = if outcome.is_none() || matches!(outcome, Some(Err(_))) {
                connection.begin_shutdown(CancelReason::Shutdown);
                connection.wait_shutdown().await.err()
            } else {
                None
            };
            // Once `close` owns this session, its task also owns the terminal and the status. A
            // prompt response racing the close handshake must not publish a competing outcome.
            if lifecycle.is_closed() {
                return;
            }
            // Matched, not taken: a `close` may have ended this turn already, and a *later* turn may
            // have started since, so an unconditional take would terminate a conversation that is not
            // this prompt's.
            if !handle.finish() {
                return;
            }
            let Some((turn, cancel_reason, closing)) = state.prepare_terminal_matching(&handle)
            else {
                return;
            };
            let cancel_failure = turn
                .cancel_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let _ = turn.approvals.flush(&turn.sink).await;
            for kind in closing {
                let _ = turn.sink.emit(kind).await;
            }
            if let Some(error) = cleanup_error {
                let _ = turn
                    .sink
                    .fail(link_failure(format!("ACP process cleanup failed: {error}")))
                    .await;
                // The process may still be live. Keep this generation installed so admission
                // remains closed until a later session close can retry teardown.
                return;
            }
            match outcome {
                None => {
                    if let Some(message) = cancel_failure {
                        let _ = turn.sink.fail(link_failure(message)).await;
                    } else if !turn.sink.is_terminal() {
                        let _ = turn
                            .sink
                            .cancel(cancel_reason.unwrap_or(CancelReason::Requested))
                            .await;
                    }
                }
                Some(Ok(response)) if reducer::was_cancelled(response.stop_reason) => {
                    let reason = cancel_reason.unwrap_or(CancelReason::Requested);
                    let _ = turn.sink.cancel(reason).await;
                }
                Some(Ok(_)) => {
                    let _ = turn.sink.complete().await;
                }
                Some(Err(error)) => {
                    if let Some(reason) = cancel_reason {
                        let _ = turn.sink.cancel(reason).await;
                    } else {
                        let message = if agent_client_protocol::is_incoming_transport_closed(&error)
                        {
                            with_stderr(&error.message, control.as_ref())
                        } else {
                            error.message.clone()
                        };
                        let _ = turn
                            .sink
                            .fail(link_failure(format!("{}: {message}", profile.id)))
                            .await;
                    }
                }
            }
            state.release_turn_matching(&handle);
        });

        Ok(stream)
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        self.require_capability(Capability::InteractiveApprovals)?;
        let Some(turn) = self.connection_state.turn() else {
            return Ok(());
        };
        self.connection_state.answer(&response)?;
        turn.approvals.flush(&turn.sink).await
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        // Recorded before the notification goes out: the agent answers the prompt with
        // `stop_reason: cancelled`, and the task that ends the turn has to report *this* reason
        // rather than flattening a shutdown or a withdrawn consent into "requested".
        // Required, not merely tidy. ACP v1 states that a client sending `session/cancel` **MUST**
        // answer every pending `session/request_permission` with the `Cancelled` outcome — twice, on
        // `RequestPermissionOutcome::Cancelled` and on `AgentRequest::RequestPermissionRequest`. An
        // agent whose permission await is not itself cancellation-aware never returns from its tool
        // call otherwise, so `session/prompt` never answers, the turn emits no terminal at all, and
        // the turn slot stays occupied for the life of the session.
        let _starting = self.connection_state.lock_turn_start();
        let Some(turn) = self.connection_state.turn() else {
            return Ok(());
        };
        if !self.connection_state.begin_cancellation(reason) {
            return Ok(());
        }
        // A notification, so there is no answer to wait for and no deadline to apply: `session/cancel`
        // is queued to the transport and the agent reports the outcome on the prompt response. The
        // start guard holds new prompts back until this notification is queued, so it cannot cancel
        // a later ACP prompt on the same session.
        let result = self
            .connection
            .connection()
            .send_notification(CancelNotification::new(self.native_session_id.clone()))
            .map_err(|_| Error::Link {
                peer: format!("ACP agent {}", self.profile.id),
                message: String::from("a link that could not carry session/cancel"),
            });
        if result.is_err() {
            self.connection_state
                .record_cancel_failure(&turn, "ACP session/cancel could not be queued");
        }
        result
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        // Claim the core lifecycle before the local prompt-start gate. A start either reserves and
        // submits under that same lifecycle transition, or observes this close before it can gain
        // authority over the ACP session.
        let claimed = {
            let mut lifecycle = self.lifecycle.lock();
            lifecycle.close()
        };
        if claimed && self.close.claim() {
            let close = Arc::clone(&self.close);
            let state = Arc::clone(&self.connection_state);
            let session_state = self.session_state.clone();
            let connection = Arc::clone(&self.connection);
            let profile = Arc::clone(&self.profile);
            let native_session_id = self.native_session_id.clone();
            let capabilities = self.agent_capabilities.clone();
            let limits = *self.host.limits();
            tokio::spawn(async move {
                finish_close(
                    close,
                    state,
                    session_state,
                    connection,
                    profile,
                    native_session_id,
                    capabilities,
                    limits,
                    reason,
                )
                .await;
            });
        }
        // A later close cannot report success until it has observed the first owner's result. The
        // lifecycle gate above keeps new prompt admission closed even when that result is failure.
        self.close.wait().await
    }

    async fn list_native_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        list_sessions(
            &self.connection,
            &self.profile,
            &self.host,
            &self.agent_capabilities,
            query,
        )
        .await
    }

    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        // ACP v1 reports context usage per session (`usage_update`, which reaches a host as
        // `ThreadUsage`) and nothing about an account's plan quota. Absence is unknown, not zero.
        Err(Error::not_supported(Capability::AccountUsage))
    }
}

/// Settles a prompt whose follow-up `session/cancel` could not be queued, in a task of its own.
///
/// The prompt is already on the wire and owns a stream, so its terminal has exactly one place to
/// go — and the peer is being shut down underneath it. Both halves of that ending, the terminal
/// commit and the release of the ACP prompt slot, must survive a caller that drops the
/// [`Session::start_turn`] future it was returned from; a slot left installed would refuse every
/// later prompt as [`Error::Busy`] on a session whose terminal never came.
fn detach_retry_cancel_cleanup(
    connection: Arc<ConnectionHandle>,
    state: Arc<client::SessionState>,
    handle: client::TurnHandle,
) {
    tokio::spawn(async move {
        let cleanup_error = connection.wait_shutdown().await.err();
        let Some((turn, _, _)) = state.prepare_terminal_matching(&handle) else {
            return;
        };
        if !turn.finish() {
            return;
        }
        let message = cleanup_error.map_or_else(
            || String::from("ACP session/cancel could not be queued after prompt admission"),
            |error| format!("ACP process cleanup failed: {error}"),
        );
        let _ = turn.sink.fail(link_failure(message)).await;
        state.release_turn_matching(&handle);
    });
}

/// Completes the one close operation after it has claimed the session lifecycle.
///
/// The task owns all awaits so dropping any caller's [`Session::close`] future cannot strand a
/// native turn, a child process, or the observable session status.
#[allow(
    clippy::too_many_arguments,
    reason = "one owned close operation needs its session facts"
)]
async fn finish_close(
    close: Arc<CloseState>,
    state: Arc<client::SessionState>,
    session_state: mango_external_agents::SessionState,
    connection: Arc<ConnectionHandle>,
    profile: Arc<AcpProfile>,
    native_session_id: AcpSessionId,
    capabilities: AgentCapabilities,
    limits: mango_external_agents::Limits,
    reason: CloseReason,
) {
    session_state.set_status(SessionStatus::Closing);
    let ending = {
        let _starting = state.lock_turn_start();
        state.begin_cancellation(reason.into());
        state.turn()
    };
    // Pending questions are a protocol debt, including questions raised while `session/close` is
    // in flight. Withdraw them before and after the bounded handshake.
    state.withdraw_pending();
    if capabilities.session_capabilities.close.is_some() {
        let _ = tokio::time::timeout(
            limits.shutdown_timeout,
            client::send(
                &connection,
                profile.as_ref(),
                limits.request_timeout,
                "session/close",
                CloseSessionRequest::new(native_session_id),
            ),
        )
        .await;
        state.withdraw_pending();
    }

    connection.begin_shutdown(reason.into());
    let result = connection.wait_shutdown().await;
    if let Some(handle) = ending
        && let Some((turn, _, closing)) = state.prepare_terminal_matching(&handle)
    {
        // Terminal commitment is reserved in the core stream, so neither a full transcript nor a
        // dropped initiating close future can prevent this task from settling its owned turn.
        if turn.finish() {
            let _ = turn.approvals.flush(&turn.sink).await;
            for kind in closing {
                let _ = turn.sink.emit(kind).await;
            }
            match &result {
                Ok(()) => {
                    let _ = turn.sink.cancel(reason.into()).await;
                }
                Err(error) => {
                    let _ = turn
                        .sink
                        .fail(link_failure(format!("ACP process cleanup failed: {error}")))
                        .await;
                }
            }
        }
        state.release_turn_matching(&handle);
    }

    if result.is_ok() {
        session_state.set_status(SessionStatus::Closed);
    }
    close.finish(result);
}

impl AcpSession {
    /// The two ids, for a caller holding a concrete session.
    #[must_use]
    pub fn session_ids(&self) -> SessionIds {
        self.session_state.snapshot().ids.clone()
    }

    /// Shares explicit-close ownership with the connection-loss watcher.
    pub(crate) fn close_state(&self) -> Arc<CloseState> {
        Arc::clone(&self.close)
    }
}

#[cfg(test)]
mod tests {
    use super::{accepted_axes, refuse_unsupported_reset};
    use mango_external_agents::configuration::{
        Configuration, ConfigurationChange, ConfigurationPatch,
    };
    use mango_external_agents::{ApprovalRouting, Error, PermissionLevel};

    /// ACP has no vendor-side "put it back": neither the mode a session opens under nor the local
    /// routing decision can be un-set, so a reset is refused the same way everywhere it is asked.
    #[test]
    fn a_patch_asking_to_reset_an_axis_is_refused_by_name() {
        let error = refuse_unsupported_reset(
            &ConfigurationPatch::new()
                .level(ConfigurationChange::Reset)
                .model(ConfigurationChange::Reset),
        )
        .expect_err("expected a refusal, received acceptance");
        assert!(
            matches!(&error, Error::HostConfiguration { received, .. } if received.contains("model, level")),
            "expected the refusal to name every axis that asked, received {error:?}"
        );
    }

    #[test]
    fn a_patch_that_only_sets_axes_is_accepted() {
        refuse_unsupported_reset(
            &ConfigurationPatch::new().level(ConfigurationChange::Set(PermissionLevel::ReadOnly)),
        )
        .expect("expected a set-only patch to be accepted");
    }

    /// Only level and routing are ever reported as accepted: this harness never encodes a model, a
    /// reasoning effort or a vendor-native option onto the ACP wire, so claiming one landed would be
    /// a claim this harness cannot back up.
    #[test]
    fn accepted_axes_keeps_only_what_this_harness_really_encodes() {
        let configuration = Configuration::unknown()
            .with_model("opus")
            .with_level(PermissionLevel::ReadOnly)
            .with_routing(ApprovalRouting::AutoReview);
        let accepted = accepted_axes(&configuration);
        assert_eq!(accepted.level, Some(PermissionLevel::ReadOnly));
        assert_eq!(accepted.routing, Some(ApprovalRouting::AutoReview));
        assert_eq!(
            accepted.model, None,
            "expected the model to stay unaccepted: nothing here ever encodes one"
        );
    }
}
