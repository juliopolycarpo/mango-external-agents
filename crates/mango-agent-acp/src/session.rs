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
    SessionId as AcpSessionId, SetSessionModeRequest,
};
use mango_external_agents::configuration::{
    Configuration, ConfigurationPatch, refuse_unsupported_native,
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
                refuse_unsupported_native(patch)?;
                refuse_unsupported_reset(patch)?;
                base.patched(patch)
            }
            None => base,
        };
        refuse_model_selection(&configuration)?;
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
    /// `accepted` carries only the axes ACP mode selection can honour — level and routing — never
    /// the full merged configuration: model and reasoning effort are refused upstream, and a
    /// vendor-native option this harness has no catalog for is never encoded onto the wire, so
    /// claiming either was "accepted" would be a claim this harness cannot back up.
    fn publish_configuration(&self, configuration: &Configuration) {
        let state = self.session_state.snapshot().configuration.clone();
        self.session_state.set_configuration(
            state
                .with_requested(configuration.clone())
                .with_accepted(accepted_axes(configuration)),
        );
    }
}

/// The subset of a [`Configuration`] this harness genuinely encodes: level, through
/// `session/set_mode`, and routing, which decides locally who answers an approval. Everything else
/// a patch could carry — model, reasoning effort, a vendor-native option — is refused before
/// it reaches here; see
/// [`AcpSession::publish_configuration`].
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

#[async_trait::async_trait]
impl Session for AcpSession {
    fn state(&self) -> &mango_external_agents::SessionState {
        &self.session_state
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
            self.connection.begin_shutdown(CancelReason::Shutdown);
            let cleanup_error = self.connection.wait_shutdown().await.err();
            if let Some((turn, _, _)) = self.connection_state.prepare_terminal_matching(&handle)
                && turn.finish()
            {
                let message = cleanup_error.map_or_else(
                    || {
                        String::from(
                            "ACP session/cancel could not be queued after prompt admission",
                        )
                    },
                    |error| format!("ACP process cleanup failed: {error}"),
                );
                let _ = turn.sink.fail(link_failure(message)).await;
                self.connection_state.release_turn_matching(&handle);
            }
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
    use super::{accepted_axes, refuse_model_selection, refuse_unsupported_reset};
    use mango_external_agents::configuration::{
        Configuration, ConfigurationChange, ConfigurationPatch,
    };
    use mango_external_agents::{ApprovalRouting, Error, PermissionLevel};

    /// Dropping the field would leave a host believing it chose a model while the agent ran whatever
    /// it was configured with, which is a silent disagreement rather than a refusal.
    #[test]
    fn a_configuration_naming_a_model_is_refused_rather_than_ignored() {
        let error = refuse_model_selection(&Configuration::unknown().with_model("gpt-5-codex"))
            .expect_err("expected a refusal, received acceptance");
        let Error::Protocol { received, .. } = &error else {
            panic!("received {error:?}");
        };
        assert_eq!(received, "gpt-5-codex");
    }

    #[test]
    fn a_configuration_naming_a_reasoning_effort_is_refused_the_same_way() {
        assert!(refuse_model_selection(&Configuration::unknown().with_effort("high")).is_err());
    }

    #[test]
    fn a_configuration_that_only_chooses_a_level_and_a_routing_is_accepted() {
        refuse_model_selection(&Configuration::unknown().with_level(PermissionLevel::Default))
            .expect("expected acceptance, received a refusal");
    }

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
