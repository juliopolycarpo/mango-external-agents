//! Configuration refusal and dispatch certainty through the real ACP adapter.

use super::*;
use mango_external_agents::{ConfigurationOptionId, ConfigurationValue, Dispatch};

#[tokio::test]
async fn unknown_native_open_settings_are_refused_after_catalog_discovery() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let error = refusal(
        AcpHarness::new(profile())
            .open_session(
                &host(&launcher),
                OpenSession::new("native-open").with_configuration(
                    ConfigurationPatch::new().native(
                        ConfigurationOptionId::new("vendor-setting"),
                        ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                    ),
                ),
            )
            .await,
    );
    assert!(
        matches!(error.cause(), Error::HostConfiguration { expected, received }
            if expected.contains("every requested ACP session configuration option")
                && received.contains("agent did not accept"))
    );
    assert_eq!(error.dispatch(), Dispatch::AcceptanceUnknown);
    assert_eq!(launcher.live_children(), 0);
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("session/new")),
        "expected the agent's catalog before the native option was rejected"
    );
}

#[tokio::test]
async fn native_turn_settings_are_refused_without_changing_configuration() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    let before = session.snapshot().configuration.clone();
    let written = launcher.written().len();
    let error = refusal(
        session
            .start_turn(TurnRequest::new("native-turn", "hello").with_configuration(
                ConfigurationPatch::new().native(
                    ConfigurationOptionId::new("vendor-setting"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
            ))
            .await,
    );
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
async fn an_unsupported_model_is_not_submitted() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    let written = launcher.written().len();
    let error = refusal(
        session
            .start_turn(
                TurnRequest::new("model-turn", "hello").with_configuration(
                    ConfigurationPatch::new()
                        .model(ConfigurationChange::Set(String::from("unsupported-model"))),
                ),
            )
            .await,
    );
    assert!(matches!(error.cause(), Error::Protocol { .. }));
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(launcher.written().len(), written);
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}
