//! One live ACP conversation.
//!
//! The shape of a turn on this dialect: `session/prompt` is a *request*, and its response is the
//! turn's end. Everything the host will see arrives in the meantime as `session/update`
//! notifications, so the prompt cannot be awaited inline — [`AcpSession::start_turn`] sends it, hands
//! the stream back, and a task waits for the response and ends the turn with it.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/prompt-turn>

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, ListSessionsRequest, PromptRequest,
    SessionId as AcpSessionId, SetSessionModeRequest,
};
use mango_external_agents::event::EventKind;
use mango_external_agents::session::{
    AccountUsage, CancelReason, CloseReason, Configuration, NativeSession, Session, SessionIds,
    SessionInfo, SessionPage, SessionQuery, TurnRequest,
};
use mango_external_agents::{
    Capability, Error, EventSink, HostContext, PermissionResponse, Result, TurnStream,
};

use crate::client::{
    Answered, ConnectionHandle, SessionState, TurnHandle, link_failure, with_stderr,
};
use crate::error::request_error;
use crate::profile::AcpProfile;
use crate::{content, permission, reducer};

/// How long a `session/close` is given before the transport is torn down anyway.
///
/// Short on purpose: closing is the call a host makes when it is already shutting down, and an agent
/// that will not answer must not hold that open. Ending the transport ends the session either way.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// One conversation with one ACP agent.
pub struct AcpSession {
    info: SessionInfo,
    profile: Arc<AcpProfile>,
    host: HostContext,
    state: Arc<SessionState>,
    connection: Arc<ConnectionHandle>,
    native_session_id: AcpSessionId,
    agent_capabilities: AgentCapabilities,
    closed: AtomicBool,
}

impl std::fmt::Debug for AcpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSession")
            .field("profile", &self.profile.id)
            .field("ids", &self.info.ids)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl AcpSession {
    pub(crate) fn new(
        info: SessionInfo,
        profile: Arc<AcpProfile>,
        host: HostContext,
        state: Arc<SessionState>,
        connection: Arc<ConnectionHandle>,
        native_session_id: AcpSessionId,
        agent_capabilities: AgentCapabilities,
    ) -> Self {
        Self {
            info,
            profile,
            host,
            state,
            connection,
            native_session_id,
            agent_capabilities,
            closed: AtomicBool::new(false),
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

    /// Sends one request and maps its failure.
    async fn request<Request>(
        &self,
        method: &'static str,
        request: Request,
    ) -> Result<Request::Response>
    where
        Request: agent_client_protocol::JsonRpcRequest,
        Request::Response: Send,
    {
        self.connection
            .connection()
            .send_request(request)
            .block_task()
            .await
            .map_err(|error| self.map_error(method, &error))
    }

    /// One agent failure as a core failure, with the link's own diagnosis when the link is what broke.
    fn map_error(&self, method: &'static str, error: &agent_client_protocol::Error) -> Error {
        if agent_client_protocol::is_incoming_transport_closed(error) {
            return Error::Link {
                peer: format!("ACP agent {}", self.profile.id),
                message: with_stderr(&error.message, self.connection.control().as_ref()),
            };
        }
        request_error(method, error, &self.login_hint())
    }

    /// The agent's own login command, or its documentation when it has none.
    ///
    /// A URL rather than an invented command: an agent whose sign-in happens inside an interactive
    /// session has no command to print, and printing a plausible one would send a person to a
    /// prompt that does not exist.
    fn login_hint(&self) -> String {
        self.profile
            .login_hint
            .clone()
            .unwrap_or_else(|| format!("see {}", self.profile.docs_url))
    }

    /// The configuration one turn runs under, refusing what ACP v1 has no surface for.
    fn effective(&self, request: &TurnRequest) -> Result<Configuration> {
        let configuration = request
            .configuration
            .clone()
            .unwrap_or_else(|| self.info.effective_configuration.clone());
        refuse_model_selection(&configuration)?;
        if configuration.level != self.info.effective_configuration.level
            && self.profile.modes.for_level(configuration.level).is_some()
        {
            // A level the profile reaches through a mode would need a `session/set_mode` mid-session,
            // and this harness does not change a session's mode under a running conversation:
            // the level a host chose at `open_session` is the level the agent was configured with.
            return Err(Error::Protocol {
                expected: String::from(
                    "a turn configuration whose level matches the session's: ACP modes are set when the session opens",
                ),
                received: format!("{:?}", configuration.level),
            });
        }
        Ok(configuration)
    }
}

/// Refuses a configuration naming a model or a reasoning effort.
///
/// ACP v1 has no model-selection surface: `session/new` takes a working directory and MCP servers,
/// and nothing else. Ignoring the field would leave a host believing it chose a model when the agent
/// ran whatever it was configured with — so it is refused rather than dropped, which is also why
/// [`Capabilities::model_catalog`](mango_external_agents::Capabilities) is never reported here.
pub(crate) fn refuse_model_selection(configuration: &Configuration) -> Result<()> {
    let chosen = configuration
        .model
        .as_deref()
        .or(configuration.effort.as_deref());
    match chosen {
        None => Ok(()),
        Some(value) => Err(Error::Protocol {
            expected: String::from(
                "no model or reasoning effort: ACP v1 has no model-selection surface",
            ),
            received: value.to_owned(),
        }),
    }
}

#[async_trait::async_trait]
impl Session for AcpSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }
        let configuration = self.effective(&request)?;
        let prompt = content::prompt(
            &request.input,
            &request.attachments,
            &self.agent_capabilities.prompt_capabilities,
        )?;

        let (sink, events) = EventSink::new(
            self.info.ids.session_id.clone(),
            request.turn_id.clone(),
            Arc::clone(self.host.clock()),
            self.host.limits().turn_channel_capacity,
        );
        self.state.begin_turn(TurnHandle {
            sink: sink.clone(),
            level: configuration.level,
        })?;

        // Emitted before the prompt is sent, so the first event a host reads names the conversation
        // the rest of the turn belongs to.
        if let Err(error) = sink
            .emit(EventKind::SessionStarted {
                native_session_id: self.native_session_id.to_string(),
                resumed: self.info.resumed,
            })
            .await
        {
            self.state.end_turn();
            return Err(error);
        }

        let sent = self
            .connection
            .connection()
            .send_request(PromptRequest::new(self.native_session_id.clone(), prompt));
        let native_turn_id = sent.id().to_string();

        let state = Arc::clone(&self.state);
        let connection = Arc::clone(&self.connection);
        let profile = Arc::clone(&self.profile);
        // Spawned rather than awaited: the response *is* the turn's end, and a host that could not
        // read its events until then would receive the whole turn at once or not at all.
        // `block_task` is sound here for the same reason — this task is not the dispatch loop.
        tokio::spawn(async move {
            let outcome = sent.block_task().await;
            let Some(turn) = state.end_turn() else {
                // Already ended: a cancel or a close got here first.
                return;
            };
            for kind in state.finish_reducing() {
                if turn.sink.emit(kind).await.is_err() {
                    return;
                }
            }
            match outcome {
                Ok(response) if reducer::was_cancelled(response.stop_reason) => {
                    let reason = state
                        .take_cancel_reason()
                        .unwrap_or(CancelReason::Requested);
                    let _ = turn.sink.cancel(reason).await;
                }
                Ok(_) => {
                    let _ = turn.sink.complete().await;
                }
                Err(error) => {
                    let message = if agent_client_protocol::is_incoming_transport_closed(&error) {
                        with_stderr(&error.message, connection.control().as_ref())
                    } else {
                        error.message.clone()
                    };
                    let _ = turn
                        .sink
                        .fail(link_failure(format!("{}: {message}", profile.id)))
                        .await;
                }
            }
        });

        Ok(TurnStream {
            turn_id: request.turn_id,
            native_turn_id,
            events,
        })
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        if self.state.answer(&response)? == Answered::AlreadyResolved {
            // The question was settled before this answer arrived, and the decision the agent acted
            // on has already been reported. A second `ApprovalResolved` would put a decision in the
            // transcript that never reached the agent.
            return Ok(());
        }
        let Some(turn) = self.state.turn() else {
            return Ok(());
        };
        turn.sink
            .emit(EventKind::ApprovalResolved {
                request_id: response.request_id,
                decision: mango_external_agents::permission::ApprovalDecision {
                    option_id: response.option_id,
                    source: response.source,
                },
            })
            .await
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        if self.state.turn().is_none() {
            return Ok(());
        }
        // Recorded before the notification goes out: the agent answers the prompt with
        // `stop_reason: cancelled`, and the task that ends the turn has to report *this* reason
        // rather than flattening a shutdown or a withdrawn consent into "requested".
        self.state.record_cancel_reason(reason);
        self.connection
            .connection()
            .send_notification(CancelNotification::new(self.native_session_id.clone()))
            .map_err(|error| self.map_error("session/cancel", &error))
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        // Every question the agent is still waiting on is withdrawn first, while the transport is
        // still up. Dropping a `Responder` sends nothing on a non-batch request, so an agent whose
        // question went unanswered would wait for a client that has gone.
        for (_, responder) in self.state.take_pending() {
            let _ = responder.respond(permission::cancelled());
        }

        if self.agent_capabilities.session_capabilities.close.is_some() {
            let _ = tokio::time::timeout(
                CLOSE_GRACE,
                self.request(
                    "session/close",
                    CloseSessionRequest::new(self.native_session_id.clone()),
                ),
            )
            .await;
        }

        // Taken out in a statement of its own: an `if let` scrutinee's guard lives through the body,
        // so cancelling under it would hold the turn lock across an `emit` that a host which stopped
        // reading parks indefinitely — and the calls waiting on that lock are the ones a host uses
        // to get out of it.
        let turn = self.state.end_turn();
        if let Some(turn) = turn {
            // Bounded, because the host is the one that may not be reading. A host that abandoned a
            // turn's stream and then closed the session must not hang its own shutdown on its own
            // backpressure; dropping the sink afterwards ends that stream instead of leaving it open
            // forever. The wait is only ever reached when the channel is already full and unread.
            let _ = tokio::time::timeout(CLOSE_GRACE, turn.sink.cancel(reason.into())).await;
        }

        self.connection.shutdown(reason.into()).await;
        Ok(())
    }

    async fn list_native_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        if self.agent_capabilities.session_capabilities.list.is_none() {
            return Err(Error::not_supported(Capability::SessionListing));
        }
        let mut request = ListSessionsRequest::new();
        if let Some(cursor) = query.cursor {
            request = request.cursor(cursor);
        }
        if let Some(path) = query.workspace_path {
            request = request.cwd(path);
        }
        let response = self.request("session/list", request).await?;
        Ok(SessionPage {
            sessions: response
                .sessions
                .into_iter()
                .map(|session| NativeSession {
                    native_session_id: session.session_id.to_string(),
                    title: session.title,
                    // ACP v1 has no preview field: a row carries a title and a directory, and
                    // inventing a preview would mean reading a transcript the library never reads.
                    preview: None,
                    workspace_path: Some(session.cwd.display().to_string()),
                    // `updated_at` is an RFC 3339 string on the wire and a `SystemTime` in the core.
                    // Parsing dates would mean a date crate for one optional field in a picker row,
                    // so the field is left absent rather than guessed.
                    updated_at: None,
                })
                .collect(),
            next_cursor: response.next_cursor,
            truncated: false,
        })
    }

    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        // ACP v1 reports context usage per session (`usage_update`, which reaches a host as
        // `ThreadUsage`) and nothing about an account's plan quota. Absence is unknown, not zero.
        Err(Error::not_supported(Capability::AccountUsage))
    }
}

impl AcpSession {
    /// The two ids, for a caller holding a concrete session.
    #[must_use]
    pub fn session_ids(&self) -> &SessionIds {
        &self.info.ids
    }
}

#[cfg(test)]
mod tests {
    use super::refuse_model_selection;
    use mango_external_agents::session::Configuration;
    use mango_external_agents::{Error, PermissionLevel};

    /// Dropping the field would leave a host believing it chose a model while the agent ran whatever
    /// it was configured with, which is a silent disagreement rather than a refusal.
    #[test]
    fn a_configuration_naming_a_model_is_refused_rather_than_ignored() {
        let error = refuse_model_selection(&Configuration {
            model: Some(String::from("gpt-5-codex")),
            ..Configuration::default()
        })
        .expect_err("expected a refusal, received acceptance");
        let Error::Protocol { received, .. } = &error else {
            panic!("received {error:?}");
        };
        assert_eq!(received, "gpt-5-codex");
    }

    #[test]
    fn a_configuration_naming_a_reasoning_effort_is_refused_the_same_way() {
        assert!(
            refuse_model_selection(&Configuration {
                effort: Some(String::from("high")),
                ..Configuration::default()
            })
            .is_err()
        );
    }

    #[test]
    fn a_configuration_that_only_chooses_a_level_and_a_routing_is_accepted() {
        refuse_model_selection(&Configuration {
            level: PermissionLevel::Default,
            ..Configuration::default()
        })
        .expect("expected acceptance, received a refusal");
    }
}
