//! A harness with no vendor behind it, for proving a host and the conformance suite itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::discovery::{AuthMode, AuthState, Discovery, GateVerdict, Model};
use crate::error::{Error, ErrorCode, Result, VendorError};
use crate::event::{Activity, ActivityKind, ActivityResult, ActivityStatus, EventKind, Usage};
use crate::harness::{Capabilities, Harness, HarnessDescriptor, HarnessKind, VendorInfo};
use crate::host::HostContext;
use crate::permission::{
    ApprovalDecision, ConfigurationVerdict, DecisionSource, PermissionMatrix, PermissionOption,
    PermissionOptionKind, PermissionRequest, PermissionResponse, broker_response,
};
use crate::session::{
    AccountUsage, CancelReason, CloseReason, OpenSession, Session, SessionIds, SessionInfo, Steer,
    SteerOutcome, TurnRequest,
};
use crate::stream::{EventSink, TurnStream};
use crate::transport::TransportKind;

const VENDOR: VendorInfo = VendorInfo {
    company: "Nobody",
    terms_url: "https://example.invalid/terms",
    privacy_url: "https://example.invalid/privacy",
    skills_are_slash_commands: false,
};

/// A [`Harness`] that answers from memory.
///
/// It emits the shape every real harness emits — a session start, a command catalog, text, an
/// approval that waits for an answer, an activity, usage and a completion — so a host can be
/// written and tested before any vendor CLI exists, and so the conformance suite has something to
/// prove itself against.
#[derive(Clone, Debug)]
pub struct FakeHarness {
    descriptor: Arc<HarnessDescriptor>,
    asks_for_approval: bool,
}

impl Default for FakeHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHarness {
    /// A harness that asks for one approval per turn.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::testing::FakeHarness;
    /// use mango_external_agents::{Harness, HarnessKind};
    ///
    /// let harness = FakeHarness::new();
    /// assert_eq!(harness.descriptor().kind, HarnessKind::Claude);
    /// ```
    pub fn new() -> Self {
        Self {
            descriptor: Arc::new(HarnessDescriptor {
                kind: HarnessKind::Claude,
                vendor: VENDOR,
                capabilities: Capabilities {
                    structured_streaming: true,
                    interactive_approvals: true,
                    resume: true,
                    usage_reporting: true,
                    cancellation: true,
                    steering: true,
                    ..Capabilities::none()
                },
                transports: &[TransportKind::Stdio],
                vendor_environment_keys: &["FAKE_AGENT_CONFIG_DIR"],
            }),
            asks_for_approval: true,
        }
    }

    /// The same harness with turns that never ask for anything.
    #[must_use]
    pub fn without_approvals(mut self) -> Self {
        self.asks_for_approval = false;
        self
    }
}

#[async_trait::async_trait]
impl Harness for FakeHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    fn permission_matrix(&self) -> PermissionMatrix {
        PermissionMatrix::build(|_, _| ConfigurationVerdict::supported())
    }

    async fn probe(&self, _host: &HostContext) -> Result<Discovery> {
        Ok(Discovery {
            executable: Some("/nowhere/fake-agent".into()),
            version: Some(String::from("0.1.0")),
            gate: GateVerdict::Usable,
            auth: AuthState::LoggedIn {
                mode: AuthMode::Subscription,
            },
            capabilities: self.descriptor.capabilities,
            models: vec![Model::new("fake-default")],
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        let resumed = request.resume.is_some();
        let native_session_id = request.resume.as_ref().map_or_else(
            || String::from("fake-session-1"),
            |resume| resume.native_session_id.clone(),
        );

        Ok(Box::new(FakeSession {
            info: SessionInfo {
                ids: SessionIds {
                    session_id: request.session_id,
                    native_session_id,
                },
                resumed,
                fallback_reason: None,
                effective_configuration: request.configuration,
                capabilities: self.descriptor.capabilities,
            },
            host: host.clone(),
            asks_for_approval: self.asks_for_approval,
            pending: Mutex::new(None),
            turns: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }))
    }
}

/// The session [`FakeHarness`] opens.
struct FakeSession {
    info: SessionInfo,
    host: HostContext,
    asks_for_approval: bool,
    pending: Mutex<Option<PendingTurn>>,
    turns: AtomicU64,
    closed: AtomicBool,
}

struct PendingTurn {
    sink: EventSink,
    approval: Option<PermissionRequest>,
}

fn approval_request(expires_at: std::time::SystemTime) -> PermissionRequest {
    PermissionRequest {
        id: String::from("fake-approval-1"),
        kind: ActivityKind::Command,
        title: String::from("Run `cargo test`"),
        detail: Some(String::from("cargo test --workspace")),
        options: vec![
            PermissionOption::new("allow", PermissionOptionKind::AllowOnce).with_label("Allow"),
            PermissionOption::new("allow-always", PermissionOptionKind::AllowAlways)
                .with_label("Allow for this session"),
            PermissionOption::new("deny", PermissionOptionKind::RejectOnce).with_label("Deny"),
        ],
        expires_at,
        truncated: false,
    }
}

impl FakeSession {
    async fn finish(&self, option_id: &str, source: DecisionSource) -> Result<()> {
        let Some(turn) = self.pending.lock().await.take() else {
            return Ok(());
        };
        turn.sink
            .emit(EventKind::ApprovalResolved {
                request_id: turn
                    .approval
                    .map_or_else(|| String::from("fake-approval-1"), |request| request.id),
                decision: ApprovalDecision {
                    option_id: option_id.to_owned(),
                    source,
                },
            })
            .await?;
        if option_id == "deny" {
            turn.sink
                .emit(EventKind::ActivityCompleted {
                    call_id: String::from("call-1"),
                    result: ActivityResult {
                        status: ActivityStatus::Cancelled,
                        detail: Some(String::from("refused")),
                        truncated: false,
                    },
                })
                .await?;
        } else {
            turn.sink
                .emit(EventKind::ActivityCompleted {
                    call_id: String::from("call-1"),
                    result: ActivityResult {
                        status: ActivityStatus::Completed,
                        detail: Some(String::from("3 passed")),
                        truncated: false,
                    },
                })
                .await?;
        }
        turn.sink
            .emit(EventKind::Usage {
                usage: Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(34),
                    ..Usage::default()
                },
            })
            .await?;
        turn.sink.complete().await
    }
}

#[async_trait::async_trait]
impl Session for FakeSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }
        let turn = self.turns.fetch_add(1, Ordering::Relaxed) + 1;
        let (sink, events) = EventSink::new(
            self.info.ids.session_id.clone(),
            request.turn_id.clone(),
            Arc::clone(self.host.clock()),
            self.host.limits().turn_channel_capacity,
        );

        sink.emit(EventKind::SessionStarted {
            native_session_id: self.info.ids.native_session_id.clone(),
            resumed: self.info.resumed,
        })
        .await?;
        sink.emit(EventKind::CommandsAvailable {
            commands: vec![crate::event::Command {
                name: String::from("review"),
                description: Some(String::from("Reviews the diff")),
            }],
        })
        .await?;
        sink.emit(EventKind::TextDelta {
            text: format!("working on {}", request.input),
        })
        .await?;

        if !self.asks_for_approval {
            sink.complete().await?;
            return Ok(TurnStream {
                turn_id: request.turn_id,
                native_turn_id: format!("fake-turn-{turn}"),
                events,
            });
        }

        sink.emit(EventKind::ActivityStarted {
            call_id: String::from("call-1"),
            activity: Activity {
                name: String::from("Bash"),
                kind: ActivityKind::Command,
                title: String::from("cargo test"),
                detail: None,
                truncated: false,
            },
        })
        .await?;

        let request_for_approval = approval_request(self.host.now() + Duration::from_secs(300));
        *self.pending.lock().await = Some(PendingTurn {
            sink: sink.clone(),
            approval: Some(request_for_approval.clone()),
        });

        // A host policy answers first when it has one; otherwise the question reaches the host.
        match broker_response(self.host.broker(), &request_for_approval).await {
            Some(response) => {
                sink.emit(EventKind::ApprovalRequested {
                    request: request_for_approval,
                })
                .await?;
                self.finish(&response.option_id, response.source).await?;
            }
            None => {
                sink.emit(EventKind::ApprovalRequested {
                    request: request_for_approval,
                })
                .await?;
            }
        }

        Ok(TurnStream {
            turn_id: request.turn_id,
            native_turn_id: format!("fake-turn-{turn}"),
            events,
        })
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        self.finish(&response.option_id, response.source).await
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        let Some(turn) = self.pending.lock().await.take() else {
            return Ok(());
        };
        turn.sink.cancel(reason).await
    }

    async fn close(&self, _reason: CloseReason) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        if let Some(turn) = self.pending.lock().await.take() {
            // A close ends whatever was running, for the same reason.
            let _ = turn.sink.cancel(CancelReason::Shutdown).await;
        }
        Ok(())
    }

    async fn steer(&self, steer: Steer) -> Result<SteerOutcome> {
        let pending = self.pending.lock().await;
        match pending.as_ref() {
            Some(turn) => {
                turn.sink
                    .emit(EventKind::TextDelta {
                        text: format!(" and {}", steer.input),
                    })
                    .await?;
                Ok(SteerOutcome::Accepted)
            }
            None => Ok(SteerOutcome::Rejected {
                reason: crate::session::SteerRejection::TurnAlreadyCompleted,
            }),
        }
    }

    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        Err(Error::Vendor(VendorError::new(
            ErrorCode::from_static("fake-no-account-surface"),
            "this harness has no account surface",
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::FakeHarness;
    use crate::event::EventKind;
    use crate::harness::Harness;
    use crate::host::HostContext;
    use crate::session::{OpenSession, TurnRequest};
    use crate::testing::FakeLauncher;
    use std::sync::Arc;

    fn host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a context")
    }

    #[tokio::test]
    async fn a_turn_stops_at_its_approval_until_it_is_answered() {
        let harness = FakeHarness::new();
        let host = host();
        let session = harness
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        let mut kinds = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            let asked = matches!(event.kind, EventKind::ApprovalRequested { .. });
            kinds.push(event.kind);
            if terminal || asked {
                break;
            }
        }
        assert!(
            matches!(kinds.last(), Some(EventKind::ApprovalRequested { .. })),
            "expected the turn to stop at its approval, received {kinds:?}"
        );

        let EventKind::ApprovalRequested { request } = kinds.pop().expect("expected the approval")
        else {
            panic!("expected an approval");
        };
        session
            .respond(request.allow().expect("expected an allow"))
            .await
            .expect("expected the answer to land");

        let mut ended = false;
        while let Some(event) = turn.recv().await {
            ended = event.is_terminal();
        }
        assert!(ended, "expected the turn to end once answered");
    }

    #[tokio::test]
    async fn a_harness_without_approvals_completes_on_its_own() {
        let harness = FakeHarness::new().without_approvals();
        let host = host();
        let session = harness
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");

        let mut last = None;
        while let Some(event) = turn.recv().await {
            last = Some(event.kind);
        }
        assert_eq!(last, Some(EventKind::Completed));
    }
}
