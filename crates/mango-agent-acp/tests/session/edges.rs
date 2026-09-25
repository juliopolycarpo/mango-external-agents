//! Wire shapes an agent may send that the happy path never exercises.

use super::*;

/// The Cursor tool call id the live build emits: an embedded newline and 85 code points.
const CURSOR_CALL_ID: &str =
    "call-e84f3ea5-ca51-4f67-968b-39323e395738-0\nfc_032f9c24-82e5-9f09-98a7-0f1da99ed55a_0";

/// A frame on the wire naming the fake agent's own session.
fn update_frame(update: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": { "sessionId": "sess_fake", "update": update }
    })
    .to_string()
}

/// Opens a session on an agent whose frames a test injects after the turn starts.
async fn open_silent() -> (
    Box<dyn Session>,
    FakeLauncher,
    mango_external_agents::testing::Announcer,
) {
    let announcer = mango_external_agents::testing::Announcer::new();
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .process()
            .announcing(announcer.clone()),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-edges").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    (session, launcher, announcer)
}

/// Reads events until one matches, failing with what arrived instead of hanging.
async fn until(turn: &mut TurnStream, wanted: impl Fn(&EventKind) -> bool) -> Vec<EventKind> {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = turn.recv().await {
            let hit = wanted(&event.kind);
            seen.push(event.kind);
            if hit {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(found, Ok(true)),
        "expected a matching event, received {seen:?}"
    );
    seen
}

/// Cursor's live call ids carry a newline and run to 85 code points. They are opaque ids under the
/// core's 128-code-point bound, so both ends of the bracket carry the agent's id verbatim.
#[tokio::test]
async fn a_newline_bearing_cursor_call_id_yields_a_matched_bracket() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().with_updates(vec![
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": CURSOR_CALL_ID,
                "title": "`echo hello-from-acp`",
                "kind": "execute",
                "status": "pending"
            }),
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": CURSOR_CALL_ID,
                "status": "completed"
            }),
        ]),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "echo"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let started = events.iter().find_map(|event| match event {
        EventKind::ActivityStarted { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    });
    let completed = events.iter().find_map(|event| match event {
        EventKind::ActivityCompleted { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    });
    assert_eq!(started, Some(CURSOR_CALL_ID), "received {events:?}");
    assert_eq!(completed, Some(CURSOR_CALL_ID), "received {events:?}");
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "received {events:?}"
    );
}

/// ACP is versioned apart from any agent, so a `sessionUpdate` variant from a later revision, or an
/// update that is not an object at all, is not a reason to fail the turn: it is skipped and the
/// frames after it still arrive.
#[tokio::test]
async fn an_unknown_or_malformed_session_update_is_ignored() {
    let (session, _launcher, announcer) = open_silent().await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    announcer.announce(update_frame(
        serde_json::json!({ "sessionUpdate": "brand_new_variant", "payload": { "x": 1 } }),
    ));
    announcer.announce(update_frame(serde_json::json!(7)));
    announcer.announce(update_frame(serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "after" }
    })));
    let seen = until(
        &mut turn,
        |event| matches!(event, EventKind::TextDelta { text } if text == "after"),
    )
    .await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, EventKind::Error { .. })),
        "expected no failure for an unknown frame, received {seen:?}"
    );
    assert_eq!(
        session.snapshot().status,
        SessionStatus::Ready,
        "expected the session to survive"
    );
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");
    let _ = drain(&mut turn).await;
    session.close(CloseReason::Shutdown).await.expect("close");
}

/// A `session/request_permission` with no params names no options, so there is nothing a host
/// could answer. It is refused on the wire, never shown, and the turn carries on.
#[tokio::test]
async fn a_permission_request_without_params_is_refused() {
    let (session, launcher, announcer) = open_silent().await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    announcer.announce(
        serde_json::json!({ "jsonrpc": "2.0", "id": 7778, "method": "session/request_permission" })
            .to_string(),
    );
    let answer = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(line) = launcher
                .written()
                .into_iter()
                .find(|line| line.contains("\"id\":7778"))
            {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected the param-less request to be answered");
    let answer: serde_json::Value = serde_json::from_str(&answer).expect("expected JSON-RPC");
    assert!(
        answer.get("error").is_some()
            || answer.pointer("/result/outcome/outcome") == Some(&serde_json::json!("cancelled")),
        "expected an error or a cancelled outcome, received {answer}"
    );
    announcer.announce(update_frame(serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "still going" }
    })));
    let seen = until(&mut turn, |event| {
        matches!(event, EventKind::TextDelta { .. })
    })
    .await;
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, EventKind::ApprovalRequested { .. })),
        "expected nothing to reach the host, received {seen:?}"
    );
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");
    let _ = drain(&mut turn).await;
    session.close(CloseReason::Shutdown).await.expect("close");
}

/// The ACP schema gives `agentCapabilities` a default of "nothing", and reads a malformed value the
/// same way. An agent that omits it or sends a non-object is therefore opened with every optional
/// surface narrowed off rather than refused, and an unknown key beside the known ones is tolerated.
#[tokio::test]
async fn agent_capabilities_absent_or_malformed_narrow_and_unknown_keys_are_tolerated() {
    for (label, capabilities, resume) in [
        ("absent", None, false),
        ("a number", Some(serde_json::json!(7)), false),
        ("a string", Some(serde_json::json!("everything")), false),
        (
            "an object with an unknown key",
            Some(serde_json::json!({ "loadSession": true, "somethingNew": { "x": 1 } })),
            true,
        ),
    ] {
        let (session, _launcher) = open(
            FakeAcpAgent::new().with_agent_capabilities(capabilities),
            permissive(),
        )
        .await;
        let snapshot = session.snapshot();
        let opened = snapshot.capabilities.capabilities();
        assert_eq!(
            opened.resume, resume,
            "expected resume {resume} for agentCapabilities {label}"
        );
        assert!(
            !opened.images && !opened.session_listing,
            "expected optional surfaces off for agentCapabilities {label}"
        );
        session.close(CloseReason::Shutdown).await.expect("close");
    }
}

/// An agent that refuses the `session/set_mode` a session opens under has not established the
/// permission level the host asked for, so opening fails rather than run under another mode, and
/// the child is ended.
#[tokio::test]
async fn a_refused_open_time_mode_fails_the_open_and_ends_the_child() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_modes(["default", "plan"])
            .refusing_set_mode(-32001, "mode rejected")
            .process(),
    );
    let profile = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );
    let result = AcpHarness::new(profile)
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-mode").with_configuration(at_level(PermissionLevel::ReadOnly)),
        )
        .await;
    let error = refusal(result);
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"session/set_mode\"")),
        "expected the mode to have been asked for"
    );
    assert!(
        matches!(error.cause(), Error::Vendor(vendor) if vendor.vendor_code.as_deref() == Some("-32001")),
        "expected the agent's own refusal, received {error:?}"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected a failed open to end the child"
    );
}

/// Cursor advertises models through a `model` config option whose ids carry their parameters
/// inline. The id is the agent's to parse, so it reaches the host catalog, the wire and the
/// accepted configuration exactly as the agent spelled it.
#[tokio::test]
async fn a_parameterized_model_id_survives_the_catalog_verbatim() {
    const MODEL: &str = "claude-opus-5[thinking=true,context=300k,effort=high,fast=false]";
    let (session, launcher) = open(
        FakeAcpAgent::new().with_config_options(vec![serde_json::json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "auto", "options": [
                { "value": "auto", "name": "Auto" },
                { "value": MODEL, "name": "Opus 5 (thinking)" }
            ]
        })]),
        permissive(),
    )
    .await;
    let catalog = session.snapshot().catalog.clone();
    let option = catalog
        .option(&ConfigurationOptionId::new("model"))
        .expect("expected the model option in the catalog");
    assert!(
        option
            .values
            .iter()
            .any(|value| value.value == ConfigurationValue::Text(String::from(MODEL))),
        "expected the parameterized id verbatim in the catalog, received {:?}",
        option.values
    );

    let outcome = session
        .configure(ConfigurationPatch::new().model(ConfigurationChange::Set(String::from(MODEL))))
        .await
        .expect("expected the model to apply");
    assert!(outcome.is_complete(), "received {outcome:?}");
    assert!(
        launcher.written().iter().any(|line| {
            line.contains("\"session/set_config_option\"")
                && line.contains(&serde_json::json!(MODEL).to_string())
        }),
        "expected the id sent verbatim"
    );
    assert_eq!(
        session.snapshot().configuration.accepted.model.as_deref(),
        Some(MODEL)
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}
