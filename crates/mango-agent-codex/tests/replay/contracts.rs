//! Session contracts checked against the replay transport.

use super::*;
use mango_external_agents::{
    ConfigurationOptionId, ConfigurationValue, Dispatch, Error, SessionStatus,
};

#[tokio::test]
async fn close_publishes_closed_state_to_subscribers() {
    let (session, _) = open("turn").await;
    let mut updates = session.subscribe();
    let revision = session.snapshot().revision;
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
    let snapshot = session.snapshot();
    assert_eq!(snapshot.status, SessionStatus::Closed);
    assert!(snapshot.revision > revision);
    let updated = updates.changed().await.expect("expected close update");
    assert_eq!(updated.status, SessionStatus::Closed);
}

#[tokio::test]
async fn a_native_turn_setting_is_refused_before_submission() {
    let (session, launcher) = open("turn").await;
    let before = session.snapshot().configuration.clone();
    let written = launcher.written().len();
    let error = session
        .start_turn(
            TurnRequest::new("native-option", "hello").with_configuration(
                ConfigurationPatch::new().native(
                    ConfigurationOptionId::new("vendor-setting"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
            ),
        )
        .await
        .expect_err("expected unsupported native setting refusal");
    assert!(
        matches!(error.cause(), Error::Protocol { received, .. } if received.contains("vendor-setting"))
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(launcher.written().len(), written);
    assert_eq!(session.snapshot().configuration, before);
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

#[tokio::test]
async fn a_native_open_setting_is_refused_before_launch() {
    let launcher = Arc::new(FakeLauncher::new());
    let (host, launcher) = with_launcher(launcher, None);
    let result = CodexHarness::new()
        .open_session(
            &host,
            OpenSession::new("native-open").with_configuration(ConfigurationPatch::new().native(
                ConfigurationOptionId::new("vendor-setting"),
                ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
            )),
        )
        .await;
    let error = match result {
        Ok(_) => panic!("expected unsupported native opening setting refusal"),
        Err(error) => error,
    };

    assert!(
        matches!(error.cause(), Error::Protocol { received, .. } if received.contains("vendor-setting"))
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert!(launcher.launches().is_empty());
    assert!(launcher.written().is_empty());
}

#[tokio::test]
async fn an_override_invalidates_only_the_changed_observations() {
    let (session, _) = open("turn").await;
    let original = session.snapshot().configuration.observed.clone();
    assert!(
        original.model.is_some(),
        "expected the recorded opening model"
    );
    let mut turn = session
        .start_turn(
            TurnRequest::new("changed-model", "hello").with_configuration(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("replacement-model"))),
            ),
        )
        .await
        .expect("expected accepted model override");
    drain(&mut turn).await;
    let configuration = session.snapshot().configuration.clone();
    assert_eq!(configuration.observed.model, None);
    assert_eq!(
        configuration.effective_model().map(|(model, _)| model),
        Some("replacement-model")
    );
    assert_eq!(configuration.observed.level, original.level);
    assert_eq!(configuration.observed.routing, original.routing);
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

/// The successful start response names a turn that cannot be safely routed.
struct InvalidTurnIdServer {
    id: String,
}

impl InvalidTurnIdServer {
    fn respond(&self, frame: &serde_json::Value) -> Option<Vec<String>> {
        if frame["method"] != "turn/start" {
            return None;
        }
        Some(vec![serde_json::json!({
            "id": frame["id"],
            "result": {"turn": {"id": self.id, "status": "inProgress", "items": [], "error": null}}
        }).to_string()])
    }
}

#[tokio::test]
async fn an_unusable_accepted_turn_id_fails_and_reaps_the_connection() {
    for id in ["x".repeat(129), String::from("invalid\u{001b}[31mturn")] {
        let server = InvalidTurnIdServer { id };
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            Transcript::load("turn").as_process_intercepting(move |frame| server.respond(frame)),
        );
        let (host, launcher) = with_launcher(launcher, None);
        let session = CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected session");
        let error = session
            .start_turn(TurnRequest::new("invalid-id", "hello"))
            .await
            .expect_err("expected rejection of an unusable accepted turn id");
        assert!(matches!(error.cause(), Error::InvalidVendorValue { .. }));
        assert_eq!(error.dispatch(), Dispatch::Accepted);
        assert_eq!(session.snapshot().status, SessionStatus::Closed);
        assert_eq!(launcher.live_children(), 0);
    }
}

#[tokio::test]
async fn local_attachment_limits_are_not_submitted() {
    let (session, launcher) = open("turn").await;
    let written = launcher.written().len();
    let attachment = mango_external_agents::Attachment {
        id: String::from("image"),
        name: String::from("image.png"),
        mime_type: String::from("image/png"),
        kind: mango_external_agents::AttachmentKind::Image,
        bytes: Vec::new(),
    };
    let error = session
        .start_turn(
            TurnRequest::new("attachments", "hello").with_attachments(vec![
                attachment;
                mango_external_agents::session::TURN_MAX_ATTACHMENTS
                    + 1
            ]),
        )
        .await
        .expect_err("expected local attachment limit");
    assert!(matches!(error.cause(), Error::LimitExceeded { .. }));
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(launcher.written().len(), written);
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}
