//! One live ACP conversation.
//!
//! The shape of a turn on this dialect: `session/prompt` is a *request*, and its response is the
//! turn's end. Everything the host will see arrives in the meantime as `session/update`
//! notifications, so the prompt cannot be awaited inline — [`AcpSession::start_turn`] sends it, hands
//! the stream back, and a task waits for the response and ends the turn with it.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/prompt-turn>

use std::sync::Arc;
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
    Capability, Error, EventSink, HostContext, PermissionResponse, Result, SessionLifecycle,
    TurnStream,
};

use crate::client::{self, ConnectionHandle, SessionState, link_failure, with_stderr};
use crate::profile::AcpProfile;
use crate::{content, reducer};

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
    lifecycle: SessionLifecycle,
}

impl std::fmt::Debug for AcpSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSession")
            .field("profile", &self.profile.id)
            .field("ids", &self.info.ids)
            .field("closed", &self.lifecycle.is_closed())
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
            lifecycle: SessionLifecycle::default(),
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
    /// A per-turn configuration goes through exactly the checks `open_session` applies, and for the
    /// same reason: a pair this profile cannot run has to be refused rather than quietly replaced by
    /// one it can. The case that made this a real hole was a turn asking for
    /// [`PermissionLevel::FullAccess`](mango_external_agents::PermissionLevel) on a profile with no
    /// mode for it — nothing would have set a mode, nothing would have refused a request, and the
    /// turn would have run as `Default` while the host believed it had granted more.
    fn effective(&self, request: &TurnRequest) -> Result<Configuration> {
        let configuration = match &request.configuration {
            Some(overrides) => self.state.configuration().with_overrides(overrides),
            None => self.state.configuration(),
        };
        refuse_model_selection(&configuration)?;
        if let Some(level) = configuration.level {
            let routing = configuration
                .routing
                .unwrap_or(mango_external_agents::ApprovalRouting::User);
            if !crate::profile::matrix(&self.profile.modes).supports(level, routing) {
                return Err(Error::HostConfiguration {
                    expected: "a (level, routing) pair this profile supports",
                    received: format!("{level:?}/{routing:?} on {}", self.profile.id),
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
        let session_level = self.info.effective_configuration.level;
        let wanted_mode = configuration
            .level
            .and_then(|level| self.profile.modes.for_level(level));
        let session_mode = session_level.and_then(|level| self.profile.modes.for_level(level));
        if configuration.level != session_level && wanted_mode != session_mode {
            return Err(Error::Protocol {
                expected: format!(
                    "a turn whose level runs under the session's own mode {:?}: ACP modes are set when the session opens",
                    session_mode
                ),
                received: format!(
                    "{:?}, which wants mode {wanted_mode:?}",
                    configuration.level
                ),
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

    async fn configuration(&self) -> Configuration {
        self.state.configuration()
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        if self.lifecycle.is_closed() {
            return Err(Error::Closed { subject: "session" });
        }
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
        // A configuration update and the single ACP prompt slot are one transaction. A second
        // caller must not merge from an old snapshot while this one accepts its settings, then win
        // the slot later and restore the stale snapshot over the newer restriction.
        let handle = {
            let Some(_lifecycle) = self.lifecycle.begin_start() else {
                return Err(Error::Closed { subject: "session" });
            };
            let _starting = self.state.lock_turn_start();
            let configuration = self.effective(&request)?;
            let handle = self.state.begin_turn(sink.clone(), configuration.level)?;
            self.state.accept_configuration(configuration);
            handle
        };

        // Emitted before the prompt is sent, so the first event a host reads names the conversation
        // the rest of the turn belongs to.
        if let Err(error) = sink
            .emit(EventKind::SessionStarted {
                native_session_id: self.native_session_id.to_string(),
                resumed: self.info.resumed,
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
            if let Some((turn, _, _)) = self.state.end_turn_matching(&handle) {
                let _ = turn.finish();
            }
            return Err(error);
        }

        // `SessionStarted` is emitted first, which can give `cancel` a window before ACP has a
        // prompt to cancel. Re-enter the start guard while sending the wire request; if cancellation
        // already won that window, queue a second notification after the prompt so the agent applies
        // it to this turn rather than treating the earlier notification as a no-op.
        let (sent, retry_cancel_error) = {
            let Some(_lifecycle) = self.lifecycle.begin_start() else {
                return Err(Error::Closed { subject: "session" });
            };
            let _starting = self.state.lock_turn_start();
            if !self.state.can_submit_prompt(&handle) {
                return Err(Error::Closed { subject: "session" });
            }
            let sent = self
                .connection
                .connection()
                .send_request(PromptRequest::new(self.native_session_id.clone(), prompt));
            let retry_cancel_error = match self.state.is_cancelling() {
                true => self
                    .connection
                    .connection()
                    .send_notification(CancelNotification::new(self.native_session_id.clone()))
                    .err()
                    .map(|error| Error::Link {
                        peer: format!("ACP agent {}", self.profile.id),
                        message: with_stderr(&error.message, self.connection.control().as_ref()),
                    }),
                false => None,
            };
            (sent, retry_cancel_error)
        };
        let native_turn_id = sent.id().to_string();

        let state = Arc::clone(&self.state);
        let connection = Arc::clone(&self.connection);
        let profile = Arc::clone(&self.profile);
        // Spawned rather than awaited: the response *is* the turn's end, and a host that could not
        // read its events until then would receive the whole turn at once or not at all.
        // `block_task` is sound here for the same reason — this task is not the dispatch loop.
        tokio::spawn(async move {
            let outcome = sent.block_task().await;
            // Matched, not taken: a `close` may have ended this turn already, and a *later* turn may
            // have started since, so an unconditional take would terminate a conversation that is not
            // this prompt's.
            let Some((turn, cancel_reason, closing)) = state.end_turn_matching(&handle) else {
                return;
            };
            // Claimed before the closing events go out, so a `close` racing this one cannot emit a
            // second terminal into the same stream.
            if !turn.finish() {
                return;
            }
            if turn.approvals.flush(&turn.sink).await.is_err() {
                return;
            }
            for kind in closing {
                if turn.sink.emit(kind).await.is_err() {
                    return;
                }
            }
            match outcome {
                Ok(response) if reducer::was_cancelled(response.stop_reason) => {
                    let reason = cancel_reason.unwrap_or(CancelReason::Requested);
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

        if let Some(error) = retry_cancel_error {
            return Err(error);
        }

        Ok(TurnStream {
            turn_id: request.turn_id,
            native_turn_id,
            events,
        })
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        let Some(turn) = self.state.turn() else {
            return Ok(());
        };
        self.state.answer(&response)?;
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
        let _starting = self.state.lock_turn_start();
        if !self.state.begin_cancellation(reason) {
            return Ok(());
        }
        // A notification, so there is no answer to wait for and no deadline to apply: `session/cancel`
        // is queued to the transport and the agent reports the outcome on the prompt response. The
        // start guard holds new prompts back until this notification is queued, so it cannot cancel
        // a later ACP prompt on the same session.
        self.connection
            .connection()
            .send_notification(CancelNotification::new(self.native_session_id.clone()))
            .map_err(|error| Error::Link {
                peer: format!("ACP agent {}", self.profile.id),
                message: with_stderr(&error.message, self.connection.control().as_ref()),
            })
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        // Claim the core lifecycle before the local prompt-start gate. A start either reserves and
        // submits under that same lifecycle transition, or observes this close before it can gain
        // authority over the ACP session.
        let ending = {
            let mut lifecycle = self.lifecycle.lock();
            if !lifecycle.close() {
                return Ok(());
            }
            let _starting = self.state.lock_turn_start();
            self.state.begin_cancellation(reason.into());
            self.state.end_turn()
        };

        // Every question the agent is still waiting on is withdrawn first, while the transport is
        // still up: an unanswered one would leave the agent waiting for a client that has gone.
        //
        // Ordering is load-bearing. The turn is ended and marked finished *first*, before the
        // `session/close` handshake, because that handshake awaits the agent for up to `CLOSE_GRACE`
        // — and a turn still live across it is a window in which the agent can raise a fresh
        // `session/request_permission` that this library would park, or worse hand to a broker that
        // answers `Allow`. Granting a permission while the session is being torn down is backwards in
        // general and absurd under `CloseReason::ConsentRevoked`, where the machine's owner has just
        // withdrawn the permission to run the agent at all.
        //
        // Taken out in a statement of its own: an `if let` scrutinee's guard lives through the body,
        // so cancelling under it would hold the turn lock across an `emit` that a host which stopped
        // reading parks indefinitely — and the calls waiting on that lock are the ones a host uses
        // to get out of it.
        let terminal = ending
            .as_ref()
            .is_some_and(super::client::TurnHandle::finish);
        self.state.withdraw_pending();

        if self.agent_capabilities.session_capabilities.close.is_some() {
            let _ = tokio::time::timeout(
                CLOSE_GRACE,
                self.request(
                    "session/close",
                    CloseSessionRequest::new(self.native_session_id.clone()),
                ),
            )
            .await;
            // Anything the agent asked during the handshake. `on_request_permission` answers such a
            // question itself once the turn is finished, so this is the belt to that braces: nothing
            // may be left parked when the transport goes.
            self.state.withdraw_pending();
        }

        if let Some(turn) = ending
            && terminal
        {
            // The same debts the prompt's own task settles: an open reasoning block and an unfinished
            // plan activity. A turn cut short by a close owes them just as much as one that ran out.
            let closing = self.state.finish_reducing();
            // Spawned rather than awaited under a timeout. `close` must not hang on a host that
            // stopped reading its own stream — but a timeout that *dropped* this future would send
            // nothing at all: `mpsc::Sender::send` is cancel-safe, so abandoning it mid-send loses the
            // value, and the host would get a stream that just ends with no `Cancelled` and no
            // `Completed`. The core's conformance suite requires exactly one terminal. Spawned, the
            // terminal lands the moment the host reads, or is dropped when the host drops the stream.
            tokio::spawn(async move {
                if turn.approvals.flush(&turn.sink).await.is_err() {
                    return;
                }
                for kind in closing {
                    if turn.sink.emit(kind).await.is_err() {
                        return;
                    }
                }
                let _ = turn.sink.cancel(reason.into()).await;
            });
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
            level: Some(PermissionLevel::Default),
            ..Configuration::default()
        })
        .expect("expected acceptance, received a refusal");
    }
}
