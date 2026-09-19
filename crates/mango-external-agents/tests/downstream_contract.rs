//! What a host outside this crate can actually build, and what it is stopped from building.
//!
//! An integration test is compiled as its own crate, which is the only place `#[non_exhaustive]`
//! means anything: inside the defining crate it is inert. So this file is where the protected
//! surfaces are proven to still be *usable* — every request type a host has to construct is
//! reachable through a constructor or a builder, and none of them needs struct-literal syntax.
//!
//! The types deliberately left open are the ones a **harness** constructs, not a host:
//! [`Discovery`], [`HarnessDescriptor`] and [`SessionIds`] are filled in by an implementor who
//! recompiles against this crate anyway, and closing them would buy a compile error in place of a
//! field nobody had to think about. The protected set is the *input* surface —
//! [`OpenSession`], [`TurnRequest`], [`ConfigurationPatch`], [`QuestionResponse`],
//! [`PermissionRequest`], [`Interaction`], [`Activity`], [`DiscoveryReceipt`] — plus the event and
//! configuration vocabularies, which will grow.
//!
//! The other half of the boundary is the one this file demonstrates by existing: a harness written
//! outside this crate, under an identity this crate has never heard of, needs no arm in any enum
//! here.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mango_external_agents::{
    Activity, ActivityContent, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate,
    Answer, AnswerValue, ApprovalDecision, AttemptId, Capabilities, CapabilityCeiling, Command,
    Configuration, ConfigurationCatalog, ConfigurationCategory, ConfigurationChange,
    ConfigurationOption, ConfigurationOptionId, ConfigurationOptionValue, ConfigurationOutcome,
    ConfigurationPatch, ConfigurationSource, ConfigurationState, ConfigurationValue,
    ConfigurationValueType, DecisionSource, Discovery, DiscoveryReceipt, Dispatch, Error,
    ExtensionValue, Extensions, FileChange, FileChangeKind, Harness, HarnessDescriptor, HarnessId,
    HarnessIdentity, HarnessRegistry, Interaction, InteractionId, InteractionKind, McpServer,
    OpenSession, OperationRef, PermissionEffect, PermissionLevel, PermissionOption,
    PermissionRequest, PermissionRisk, PermissionScope, PlanStep, PlanStepStatus, ProfileId,
    ProtocolFamily, Question, QuestionForm, QuestionId, QuestionOption, QuestionOptionId,
    QuestionRequest, QuestionResponse, RecoveryAction, RecoveryRecord, RejectedSetting,
    RequestFingerprint, Rollback, Session, SessionCapabilities, SessionId, SessionIds,
    SessionRevision, SessionSnapshot, SessionState, SessionStatus, SettingRejection,
    TerminalStatus, TransportKind, TransportSelection, TurnId, TurnRequest, VendorInfo,
};

/// A harness this crate has never heard of, written entirely against the public API.
///
/// The point of the whole identity split: no arm was added to any enum in the library to make this
/// compile, and it registers alongside the built-in ids.
struct AcmeHarness {
    descriptor: HarnessDescriptor,
}

/// A launcher the receipt-contract test never reaches.
struct RefusingLauncher;

#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for RefusingLauncher {
    async fn spawn(
        &self,
        _spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        Err(Error::Closed {
            subject: "test launcher",
        })
    }
}

impl AcmeHarness {
    fn new() -> Self {
        Self {
            descriptor: HarnessDescriptor {
                identity: HarnessIdentity::custom("acme-agent", "acme-rpc", Some("fast"))
                    .expect("expected a valid identity"),
                vendor: VendorInfo {
                    company: "Acme",
                    terms_url: "https://acme.invalid/terms",
                    privacy_url: "https://acme.invalid/privacy",
                    skills_are_slash_commands: false,
                },
                capabilities: CapabilityCeiling::new(Capabilities {
                    structured_streaming: true,
                    ..Capabilities::none()
                }),
                transports: &[TransportKind::Stdio],
                vendor_environment_keys: &[],
            },
        }
    }
}

#[async_trait::async_trait]
impl Harness for AcmeHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    fn permission_matrix(&self) -> mango_external_agents::PermissionMatrix {
        mango_external_agents::PermissionMatrix::none(
            mango_external_agents::UnsupportedReason::NotOfferedByVendor,
        )
    }

    async fn probe(
        &self,
        _host: &mango_external_agents::HostContext,
    ) -> mango_external_agents::Result<Discovery> {
        Ok(Discovery::not_installed())
    }

    async fn open_session(
        &self,
        _host: &mango_external_agents::HostContext,
        _request: OpenSession,
    ) -> mango_external_agents::Result<Box<dyn Session>> {
        Err(Error::Closed { subject: "session" })
    }
}

/// A session written outside this crate, holding nothing but the public [`SessionState`].
struct AcmeSession {
    state: SessionState,
}

impl AcmeSession {
    fn new() -> Self {
        Self {
            state: SessionState::new(
                Arc::new(mango_external_agents::SystemClock),
                SessionSnapshot::opening(
                    SessionIds {
                        session_id: SessionId::new("chat-1"),
                        native_session_id: String::from("acme-1"),
                    },
                    HarnessIdentity::custom("acme-agent", "acme-rpc", None)
                        .expect("expected a valid identity"),
                    TransportSelection::new(Some(TransportKind::Stdio), TransportKind::Stdio),
                    SystemTime::UNIX_EPOCH,
                )
                .with_capabilities(SessionCapabilities::none())
                .with_catalog(catalog()),
            ),
        }
    }
}

#[async_trait::async_trait]
impl Session for AcmeSession {
    fn state(&self) -> &SessionState {
        &self.state
    }

    async fn start_turn(
        &self,
        _request: TurnRequest,
    ) -> mango_external_agents::Result<mango_external_agents::TurnStream> {
        Err(Error::Closed { subject: "session" })
    }

    async fn respond(
        &self,
        _response: mango_external_agents::PermissionResponse,
    ) -> mango_external_agents::Result<()> {
        Err(Error::Closed { subject: "session" })
    }

    async fn cancel(
        &self,
        _reason: mango_external_agents::CancelReason,
    ) -> mango_external_agents::Result<()> {
        Ok(())
    }

    async fn close(
        &self,
        _reason: mango_external_agents::CloseReason,
    ) -> mango_external_agents::Result<()> {
        Ok(())
    }
}

fn catalog() -> ConfigurationCatalog {
    ConfigurationCatalog::new(vec![
        ConfigurationOption::new(
            ConfigurationOptionId::new("model"),
            ConfigurationCategory::Model,
            ConfigurationValueType::Enumerated,
        )
        .with_name("Model")
        .with_values(vec![
            ConfigurationOptionValue::new(ConfigurationValue::text("acme-small")).as_default(),
            ConfigurationOptionValue::new(ConfigurationValue::text("acme-large")),
        ])
        .resettable(),
        ConfigurationOption::new(
            ConfigurationOptionId::new("telemetry"),
            ConfigurationCategory::Other(String::from("acme-only")),
            ConfigurationValueType::Boolean,
        ),
    ])
}

/// A custom native harness registers without a core enum arm, and collisions are refused.
#[test]
fn a_harness_this_crate_never_heard_of_registers_and_dispatches_by_its_own_id() {
    let harness = Arc::new(AcmeHarness::new());
    let id = harness.descriptor().id().clone();
    let registry = HarnessRegistry::new(vec![harness.clone()]).expect("expected a registry");

    assert_eq!(id.as_str(), "acme-agent");
    assert_eq!(
        registry
            .require(&id)
            .expect("expected the harness back")
            .descriptor()
            .identity
            .protocol
            .as_str(),
        "acme-rpc"
    );
    assert!(registry.get(&HarnessId::claude()).is_none());

    let collision = HarnessRegistry::new(vec![harness.clone(), harness])
        .expect_err("expected a duplicate registration to be refused");
    assert!(
        collision
            .to_string()
            .contains("acme-agent registered twice"),
        "expected the colliding id in the diagnostic, received {collision}"
    );
}

/// An identifier that could not survive a map key, a log line or a path component is refused where
/// it is built, with the offending value in the message.
#[test]
fn an_invalid_identifier_is_refused_deterministically_from_outside_the_crate() {
    for bad in ["", "Acme", "acme agent", "acme::agent", ":acme", "acme-"] {
        assert!(
            HarnessId::new(bad).is_err(),
            "expected {bad:?} to be refused"
        );
        assert!(ProtocolFamily::new(bad).is_err());
        assert!(ProfileId::new(bad).is_err());
    }
    assert!(HarnessId::new("i".repeat(65)).is_err());
    assert_eq!(
        HarnessId::acp(&ProfileId::new("cursor").expect("a profile")).as_str(),
        "acp:cursor"
    );
}

/// The whole input surface is reachable without struct-literal syntax. This is what
/// `#[non_exhaustive]` costs a host, and the cost has to stay at zero.
#[test]
fn every_protected_request_type_is_constructible_through_its_builders() {
    let receipt = DiscoveryReceipt::new(HarnessId::claude(), Discovery::not_installed(), now())
        .with_executable_fingerprint("sha256:abc")
        .with_environment_fingerprint("env-1")
        .valid_for(Duration::from_secs(120));

    let open = OpenSession::new("chat-1")
        .with_configuration(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("acme-large")))
                .level(ConfigurationChange::Set(PermissionLevel::ReadOnly))
                .effort(ConfigurationChange::Reset)
                .native(
                    ConfigurationOptionId::new("telemetry"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(false)),
                ),
        )
        .over_transport(TransportKind::Stdio)
        .with_mcp_servers(vec![McpServer::stdio("docs", "docs-mcp")])
        .with_discovery(receipt)
        .resuming("acme-1", mango_external_agents::ResumeMode::Fallback);

    assert_eq!(open.transport, Some(TransportKind::Stdio));
    assert!(open.discovery.is_some());
    assert!(open.configuration.asks_for_a_reset());

    let turn = TurnRequest::new("turn-1", "ship it")
        .as_attempt(AttemptId::new(2))
        .with_configuration(ConfigurationPatch::new().model(ConfigurationChange::Reset));
    assert_eq!(turn.attempt.get(), 2);

    let interaction = Interaction::new(
        InteractionId::new("ask-1"),
        InteractionKind::Question,
        SessionId::new("chat-1"),
        now(),
    )
    .during(OperationRef::new(
        SessionId::new("chat-1"),
        TurnId::new("turn-1"),
        AttemptId::default(),
    ));
    let question = QuestionRequest::new(
        interaction,
        vec![
            Question::new(
                QuestionId::new("branch"),
                "Which branch?",
                QuestionForm::FreeText { placeholder: None },
            )
            .required(),
            Question::new(
                QuestionId::new("tests"),
                "Run tests?",
                QuestionForm::Choice {
                    options: vec![
                        QuestionOption::new(QuestionOptionId::new("yes")).with_label("Yes"),
                        QuestionOption::new(QuestionOptionId::new("no")).with_label("No"),
                    ],
                    multi_select: false,
                },
            )
            .with_detail("after the change lands"),
        ],
    )
    .with_title("Before I start");

    let response = QuestionResponse::new(
        question.interaction.id.clone(),
        vec![
            Answer::new(QuestionId::new("branch"), AnswerValue::text("main")),
            Answer::new(
                QuestionId::new("tests"),
                AnswerValue::chosen(QuestionOptionId::new("yes")),
            ),
        ],
    );
    question
        .validate(&response)
        .expect("expected the answers to be accepted");

    let activity = Activity::new("Edit", ActivityKind::FileChange, "src/lib.rs")
        .with_detail("+10 -2")
        .with_item_id("item-1")
        .inside("call-0")
        .by_subagent("explorer")
        .with_content(ActivityContent::Diff {
            files: vec![
                FileChange::new("src/lib.rs")
                    .with_kind(FileChangeKind::Modified)
                    .with_line_counts(10, 2),
            ],
        })
        .with_extensions(
            Extensions::new().with("sandbox", ExtensionValue::text("workspace-write")),
        );
    assert_eq!(activity.subagent_id.as_deref(), Some("explorer"));

    let permission = PermissionRequest::new(
        Interaction::new(
            InteractionId::new("req-1"),
            InteractionKind::Permission,
            SessionId::new("chat-1"),
            now(),
        ),
        ActivityKind::Command,
        "Run `rm -rf build`",
        vec![
            PermissionOption::new("once", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once)
                .with_risk(PermissionRisk::Destructive)
                .with_label("Allow once"),
            PermissionOption::new("always", PermissionEffect::Allow)
                .with_scope(PermissionScope::Persistent)
                .policy_changing(),
            PermissionOption::new("no", PermissionEffect::Reject).with_scope(PermissionScope::Once),
        ],
    )
    .with_detail("in the authorised workspace");
    assert_eq!(permission.id().as_str(), "req-1");
    assert_eq!(
        permission.allow().expect("expected an allow").option_id,
        "once",
        "expected the narrowest reach on offer"
    );
}

/// Keep, set and reset stay distinguishable across the wire, and so do the three readings.
#[test]
fn configuration_round_trips_without_collapsing_keep_set_and_reset() {
    let patch = ConfigurationPatch::new()
        .model(ConfigurationChange::Set(String::from("acme-large")))
        .effort(ConfigurationChange::Reset);

    let encoded = serde_json::to_value(&patch).expect("expected a serializable patch");
    assert_eq!(encoded["model"]["op"], "set");
    assert_eq!(encoded["model"]["value"], "acme-large");
    assert_eq!(encoded["effort"]["op"], "reset");
    assert!(
        encoded.get("level").is_none(),
        "expected an untouched axis to say nothing, received {encoded}"
    );
    assert_eq!(
        serde_json::from_value::<ConfigurationPatch>(encoded).expect("expected the patch back"),
        patch
    );

    let state = ConfigurationState::new(
        Configuration::unknown().with_model("acme-large"),
        Configuration::unknown()
            .with_model("acme-large")
            .with_effort("high"),
        Configuration::unknown().with_model("acme-large-20260101"),
    );
    assert_eq!(
        state.effective_model(),
        Some(("acme-large-20260101", ConfigurationSource::Observed))
    );
    assert_eq!(
        state.effective_effort(),
        Some(("high", ConfigurationSource::Accepted))
    );
    assert_eq!(
        state.effective_level(),
        None,
        "expected an axis nobody spoke about to stay unknown"
    );

    let round_tripped: ConfigurationState =
        serde_json::from_value(serde_json::to_value(&state).expect("serializable state"))
            .expect("expected the state back");
    assert_eq!(round_tripped, state);
}

/// A catalog keeps native ids, ordering and value types, and an unknown category does not take the
/// known rows with it.
/// The answer to `Session::configure`, pinned on the wire.
///
/// It is the one configuration type a host both receives and persists — a runtime relays it to a
/// browser, and a browser renders "two of three landed" from it. That makes its field names a
/// contract, and until this test existed the only thing holding them was the struct definition.
/// A rename here is a silent change of meaning at the far end of somebody's WebSocket.
///
/// The partial case is the one worth pinning rather than the happy one: `applied` and `rejected`
/// are both `skip_serializing_if = "Vec::is_empty"`, so an outcome where everything landed encodes
/// without either key, and a test built on that would never notice a rename of either.
#[test]
fn a_partial_configuration_outcome_survives_serialization_with_its_rollback_intact() {
    let outcome = ConfigurationOutcome::applied(
        ConfigurationState::unknown(),
        vec![ConfigurationOptionId::new("model")],
    )
    .rejecting(
        vec![RejectedSetting::new(
            ConfigurationOptionId::new("effort"),
            SettingRejection::ResetNotSupported,
        )],
        Rollback::NotAttempted,
    );
    assert!(
        outcome.is_partial() && !outcome.is_complete(),
        "expected a partial outcome to build, received {outcome:?}"
    );

    let encoded = serde_json::to_value(&outcome).expect("expected a serializable outcome");
    assert_eq!(encoded["applied"][0], "model");
    assert_eq!(encoded["rejected"][0]["option"], "effort");
    assert_eq!(encoded["rejected"][0]["reason"], "reset-not-supported");
    assert_eq!(
        encoded["rollback"], "not-attempted",
        "expected the rollback to say the rest was left applied, received {:?}",
        encoded["rollback"]
    );

    let round_tripped: ConfigurationOutcome =
        serde_json::from_value(encoded).expect("expected the outcome back");
    assert_eq!(
        round_tripped, outcome,
        "expected an outcome to survive a round trip unchanged"
    );
}

/// The retry contract, exercised from outside the crate that defines it.
///
/// `RecoveryRecord` is the only public type whose whole job is to **refuse** things, so a host
/// that cannot reach its refusals from outside has no retry contract at all. Compiled here rather
/// than only in the crate's own unit tests, because `#[non_exhaustive]` and private fields are
/// inert inside the defining crate — and this is the surface a supervisor in another repository
/// actually writes its loop against.
#[test]
fn the_retry_contract_refuses_an_unsafe_replay_from_outside_this_crate() {
    let request = TurnRequest::new("turn-1", "say hello");
    let mut record = RecoveryRecord::new(SessionId::new("chat-1"), &request)
        .expect("expected a record for a serializable request");
    assert_eq!(record.action(), RecoveryAction::Submit);

    let operation = record.operation().clone();
    record
        .record_dispatch(&operation, Dispatch::AcceptanceUnknown)
        .expect("expected the pre-dispatch certainty to be recorded");
    assert_eq!(record.action(), RecoveryAction::Reconcile);
    assert!(!Dispatch::AcceptanceUnknown.is_safe_to_replay());
    assert!(Dispatch::AcceptanceUnknown.needs_reconciliation());

    // The whole point: uncertain acceptance does not become a newer attempt on its own.
    let retry = request.clone().as_attempt(AttemptId::new(2));
    let refusal = record
        .retry(&retry)
        .map(drop)
        .expect_err("expected an unreconciled replay to be refused");
    assert!(
        matches!(refusal, Error::HostConfiguration { .. }),
        "expected a typed refusal naming the expected shape, received {refusal:?}"
    );

    // Native proof of absence is the only thing that unlocks it.
    record
        .reconcile_not_submitted(&operation)
        .expect("expected proven non-submission to be recordable");
    let advanced = record.retry(&retry).expect("expected the newer attempt");
    assert_eq!(advanced.attempt, AttemptId::new(2));
    assert!(operation.is_superseded_by(&advanced));

    // Same logical turn, different content, is a different request and is refused.
    let tampered = TurnRequest::new("turn-1", "say something else").as_attempt(AttemptId::new(3));
    assert!(
        record.validate(&tampered).is_err(),
        "expected a reused logical id with changed content to be refused"
    );
    assert_ne!(
        RequestFingerprint::of(&request).expect("expected a fingerprint"),
        RequestFingerprint::of(&tampered).expect("expected a fingerprint"),
        "expected different content to produce a different fingerprint"
    );

    record
        .finish(&advanced, TerminalStatus::Completed)
        .expect("expected the terminal to commit");
    assert_eq!(record.action(), RecoveryAction::Finished);
    assert!(
        record
            .retry(&request.clone().as_attempt(AttemptId::new(4)))
            .is_err(),
        "expected a committed terminal to end recovery"
    );
}

#[test]
fn a_configuration_catalog_survives_serialization_with_its_vendor_shape_intact() {
    let catalog = catalog().normalized();
    let encoded = serde_json::to_value(&catalog).expect("expected a serializable catalog");
    assert_eq!(encoded[0]["id"], "model");
    assert_eq!(encoded[0]["category"], "model");
    assert_eq!(encoded[0]["valueType"], "enumerated");
    assert_eq!(encoded[0]["values"][0]["value"]["value"], "acme-small");
    assert_eq!(encoded[0]["values"][0]["isDefault"], true);
    assert_eq!(encoded[1]["category"]["other"], "acme-only");

    let round_tripped: ConfigurationCatalog =
        serde_json::from_value(encoded).expect("expected the catalog back");
    assert_eq!(round_tripped, catalog);
    assert_eq!(
        round_tripped
            .in_category(&ConfigurationCategory::Model)
            .len(),
        1,
        "expected the known row to stay usable next to the unknown category"
    );
}

/// Scope, risk and the policy-changing flag survive the wire. A host that lost them would show
/// "just this once" over a choice that rewrites a vendor's settings file.
#[test]
fn permission_scope_and_policy_effects_survive_serialization() {
    let option = PermissionOption::new("always", PermissionEffect::Allow)
        .with_scope(PermissionScope::Persistent)
        .with_risk(PermissionRisk::Destructive)
        .policy_changing()
        .with_label("Always allow");

    let encoded = serde_json::to_value(&option).expect("expected a serializable option");
    assert_eq!(encoded["effect"], "allow");
    assert_eq!(encoded["scope"], "persistent");
    assert_eq!(encoded["risk"], "destructive");
    assert_eq!(encoded["policyChanging"], true);
    assert_eq!(
        serde_json::from_value::<PermissionOption>(encoded).expect("expected the option back"),
        option
    );

    let decision = ApprovalDecision::from_option(&option, DecisionSource::User);
    assert!(decision.is_standing());
    let encoded = serde_json::to_value(&decision).expect("expected a serializable decision");
    assert_eq!(encoded["scope"], "persistent");
    assert_eq!(encoded["policyChanging"], true);

    // An unstated scope stays unstated on the wire, and is treated as standing rather than as the
    // narrow reading: an unmeasured reach is the one thing a host must not be told is narrow.
    let unstated = PermissionOption::new("maybe", PermissionEffect::Other);
    assert_eq!(unstated.scope, None);
    assert!(unstated.is_standing());
    assert!(
        !PermissionOption::new("once", PermissionEffect::Allow)
            .with_scope(PermissionScope::Once)
            .is_standing()
    );
}

/// A question round-trips with the vendor's own question and option ids, and a plain question is
/// never an executable grant.
#[test]
fn questions_round_trip_with_native_ids_and_grant_nothing() {
    let response = QuestionResponse::new(
        InteractionId::new("ask-1"),
        vec![Answer::new(
            QuestionId::new("branch"),
            AnswerValue::Chosen {
                option_ids: vec![QuestionOptionId::new("next")],
            },
        )],
    );
    let encoded = serde_json::to_value(&response).expect("expected a serializable response");
    assert_eq!(encoded["interactionId"], "ask-1");
    assert_eq!(encoded["answers"][0]["questionId"], "branch");
    assert_eq!(
        serde_json::from_value::<QuestionResponse>(encoded).expect("expected the response back"),
        response
    );

    assert!(!InteractionKind::Question.grants_authority());
    assert!(InteractionKind::Permission.grants_authority());
}

/// The extension channel is bounded and scalar-only. A host cannot receive an arbitrary vendor
/// document through it, and nothing in it is executable.
#[test]
fn the_extension_channel_is_bounded_scalar_only_and_redacted() {
    let extensions: Extensions = (0..64)
        .map(|index| {
            (
                format!("key{index:03}"),
                ExtensionValue::Integer(i64::from(index)),
            )
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    let bounded = extensions.normalized();
    assert_eq!(bounded.len(), 32, "expected the cap to fire");

    let redacted = Extensions::new()
        .with(
            "auth",
            ExtensionValue::text("Authorization: Bearer sk-live-abcdefghijklmnop"),
        )
        .normalized();
    let Some(ExtensionValue::Text(value)) = redacted.get("auth") else {
        panic!("expected text, received {:?}", redacted.get("auth"));
    };
    assert!(
        !value.contains("sk-live-abcdefghijklmnop"),
        "expected the token to be redacted, received {value}"
    );

    // A flat object on the wire: no nesting to walk, and nothing to re-parse as a vendor frame.
    let encoded = serde_json::to_value(&redacted).expect("expected a serializable map");
    assert!(encoded.is_object(), "received {encoded}");
}

/// Typed content keeps its identity and relationships rather than becoming prose.
#[test]
fn structured_content_keeps_its_identity_through_serialization() {
    let content = ActivityContent::Plan {
        steps: vec![
            PlanStep::new("read the reducer")
                .with_id("step-1")
                .with_status(PlanStepStatus::Completed),
            PlanStep::new("write the test").with_id("step-2"),
        ],
    };
    let encoded = serde_json::to_value(&content).expect("expected serializable content");
    assert_eq!(encoded["type"], "plan");
    assert_eq!(encoded["steps"][0]["id"], "step-1");
    assert_eq!(encoded["steps"][0]["status"], "completed");
    assert_eq!(
        serde_json::from_value::<ActivityContent>(encoded).expect("expected the content back"),
        content
    );
}

/// Both halves of an activity's later life are reachable from outside this crate.
///
/// They became `#[non_exhaustive]` when they gained `content`, which is exactly the change that
/// would have broken a host constructing them with a struct literal. The builders are the
/// replacement, and this is the only place the attribute is real, so this is where they are proved
/// to still work — including the shapes a reducer actually builds, where the title or the detail is
/// an `Option` it did not decide.
#[test]
fn an_activity_update_and_result_are_constructible_through_their_builders() {
    let update = ActivityUpdate::new()
        .with_title("applying the patch")
        .with_optional_detail(None)
        .with_content(ActivityContent::Output {
            text: String::from("2 files changed"),
        });
    assert_eq!(update.title.as_deref(), Some("applying the patch"));
    assert!(update.detail.is_none());
    assert!(!update.is_empty());

    let result = ActivityResult::new(ActivityStatus::Completed)
        .with_optional_detail(Some(String::from("exit 0")))
        .with_content(ActivityContent::Diff {
            files: vec![FileChange::new("src/lib.rs").with_line_counts(10, 2)],
        });
    assert_eq!(result.status, ActivityStatus::Completed);

    let encoded = serde_json::to_value(&result).expect("expected a serializable result");
    assert_eq!(encoded["content"]["type"], "diff");
    assert_eq!(encoded["content"]["files"][0]["addedLines"], 10);
    assert!(
        encoded["content"]["files"][0].get("kind").is_none(),
        "an unstated kind must not serialise as one, received {encoded}"
    );
}

/// Dispatch certainty is stated without promising the vendor deduplicates anything.
#[test]
fn a_failure_states_how_far_its_request_got_without_promising_idempotency() {
    assert_eq!(
        Error::not_supported(mango_external_agents::Capability::Steering)
            .with_dispatch(Dispatch::NotSubmitted)
            .dispatch(),
        Dispatch::NotSubmitted
    );
    assert!(
        Error::not_supported(mango_external_agents::Capability::Steering)
            .with_dispatch(Dispatch::NotSubmitted)
            .dispatch()
            .is_safe_to_replay()
    );
    assert_eq!(
        Error::Closed { subject: "link" }.dispatch(),
        Dispatch::AcceptanceUnknown
    );
    assert!(
        Error::Closed { subject: "link" }
            .dispatch()
            .needs_reconciliation()
    );

    // The same logical turn, two attempts: the host can tell one from the other, and can tell that
    // the later one supersedes the earlier.
    let first = OperationRef::new(
        SessionId::new("chat-1"),
        TurnId::new("turn-1"),
        AttemptId::new(1),
    );
    let retry = first.clone().retried_as(AttemptId::new(2));
    assert!(first.is_same_turn(&retry));
    assert!(first.is_superseded_by(&retry));
    assert!(!retry.is_superseded_by(&first));
}

/// Session state is readable and observable from outside the crate, with no turn anywhere.
#[tokio::test]
async fn a_downstream_session_publishes_state_with_no_turn_running() {
    let session = AcmeSession::new();

    assert_eq!(session.snapshot().revision, SessionRevision::INITIAL);
    assert_eq!(session.ids().native_session_id, "acme-1");
    assert_eq!(session.snapshot().catalog.len(), 2);
    assert!(!session.snapshot().transport.was_substituted());

    let mut subscription = session.subscribe();
    let opened_at = subscription.current().revision;

    session.state().set_commands(vec![
        Command::new("review").with_description("Reviews the diff"),
    ]);
    session.state().set_configuration(
        ConfigurationState::unknown()
            .with_observed(Configuration::unknown().with_model("acme-large")),
    );
    session.state().set_status(SessionStatus::Closing);

    let seen = subscription
        .changed()
        .await
        .expect("expected the change to reach the subscriber");
    assert!(seen.revision > opened_at);
    assert_eq!(seen.commands.len(), 1);
    assert_eq!(
        seen.configuration.observed.model.as_deref(),
        Some("acme-large")
    );
    assert_eq!(seen.status, SessionStatus::Closing);
    assert!(!seen.status.is_usable());
}

/// A host that vouches for its own probe is checked against the session it is opening, never
/// cached: the receipt travels on the request and is forgotten with it.
#[test]
fn a_discovery_receipt_states_its_own_identity_and_freshness() {
    let observed = now();
    let receipt = DiscoveryReceipt::new(HarnessId::claude(), Discovery::not_installed(), observed)
        .valid_for(Duration::from_secs(60));

    assert!(receipt.is_fresh(observed));
    assert!(receipt.is_fresh(observed + Duration::from_secs(60)));
    assert!(!receipt.is_fresh(observed + Duration::from_secs(61)));
    assert_eq!(
        receipt.age(observed + Duration::from_secs(30)),
        Some(Duration::from_secs(30))
    );
    // A clock that went backwards is reported as an age this library cannot measure, not as zero.
    assert_eq!(receipt.age(observed - Duration::from_secs(1)), None);
    assert!(!receipt.is_fresh(observed - Duration::from_secs(1)));

    let descriptor = AcmeHarness::new();
    let host = mango_external_agents::HostContext::builder()
        .launcher(Arc::new(RefusingLauncher))
        .cwd("/workspace")
        .client_info("downstream-test", "0.0.0")
        .build()
        .expect("expected a host");
    let error = receipt
        .verify_for(descriptor.descriptor(), &host, &OpenSession::new("chat-1"))
        .expect_err("expected a receipt for another harness to be refused");
    assert!(
        error
            .to_string()
            .contains("a receipt for a different harness"),
        "expected the mismatch without payloads in the diagnostic, received {error}"
    );
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// The host-driven half: a listing and an account reading, with no conversation opened first.
#[cfg(feature = "testing")]
mod with_a_host {
    use super::AcmeHarness;
    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{Capability, Error, Harness, HostContext, SessionQuery};
    use std::sync::Arc;

    fn host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("downstream-test", "0.0.0")
            .build()
            .expect("expected a host context")
    }

    /// A host drawing a picker has no conversation to open one on, so listing cannot require one.
    #[tokio::test]
    async fn listing_and_account_readings_are_callable_without_a_live_session() {
        let harness = AcmeHarness::new();
        let host = host();

        let error = harness
            .list_sessions(&host, SessionQuery::default())
            .await
            .expect_err("expected a typed refusal from a harness that does not list");
        assert!(
            matches!(
                error,
                Error::NotSupported {
                    capability: Capability::SessionListing
                }
            ),
            "received {error:?}"
        );

        let error = harness
            .account_usage(&host)
            .await
            .expect_err("expected a typed refusal from a harness that reads no account");
        assert!(
            matches!(
                error,
                Error::NotSupported {
                    capability: Capability::AccountUsage
                }
            ),
            "received {error:?}"
        );
    }

    /// A capability a harness never declared is refused before anything is spawned.
    #[tokio::test]
    async fn an_undeclared_capability_is_refused_at_the_request_that_asks_for_it() {
        use mango_external_agents::{McpServer, OpenSession, ResumeMode};

        let harness = AcmeHarness::new();
        let host = host();

        let error = harness
            .validate_open_session(
                &host,
                &OpenSession::new("chat-1").resuming("acme-1", ResumeMode::Strict),
            )
            .expect_err("expected strict resume to be refused");
        assert!(
            matches!(
                error,
                Error::NotSupported {
                    capability: Capability::Resume
                }
            ),
            "received {error:?}"
        );

        let error = harness
            .validate_open_session(
                &host,
                &OpenSession::new("chat-1")
                    .with_mcp_servers(vec![McpServer::stdio("docs", "docs-mcp")]),
            )
            .expect_err("expected MCP passthrough to be refused");
        assert!(
            error.to_string().contains("received MCP server count 1"),
            "expected the received count in the diagnostic, received {error}"
        );
    }
}
