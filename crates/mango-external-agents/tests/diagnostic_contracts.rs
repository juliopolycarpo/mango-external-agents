//! Diagnostic policy for the contract carriers added after the previous release.

use std::fmt::Debug;
use std::time::SystemTime;

use mango_external_agents::{
    ActivityContent, AttemptId, Configuration, ConfigurationCatalog, ConfigurationCategory,
    ConfigurationChange, ConfigurationOption, ConfigurationOptionId, ConfigurationOptionValue,
    ConfigurationOutcome, ConfigurationPatch, ConfigurationState, ConfigurationValue,
    ConfigurationValueType, ExtensionValue, Extensions, FileChange, FileChangeKind,
    HarnessIdentity, OperationRef, PlanStep, RejectedSetting, Rollback, SessionId, SessionIds,
    SessionSnapshot, SettingRejection, TransportKind, TransportSelection, TurnId,
};

const PAYLOAD: &str = "do-not-log-this-payload";

#[test]
fn invalid_identifier_errors_report_shape_without_replaying_input() {
    for raw in [
        format!("{PAYLOAD}="),
        format!("-{PAYLOAD}"),
        format!("{PAYLOAD}::id"),
    ] {
        for error in [
            mango_external_agents::HarnessId::new(&raw).expect_err("expected invalid harness id"),
            mango_external_agents::ProtocolFamily::new(&raw)
                .expect_err("expected invalid protocol"),
            mango_external_agents::ProfileId::new(&raw).expect_err("expected invalid profile"),
        ] {
            assert_payload_free(&error);
            assert!(!error.to_string().contains(PAYLOAD));
            assert!(error.to_string().contains("expected ASCII lowercase"));
        }
    }
}

fn assert_payload_free(value: impl Debug) {
    let diagnostic = format!("{value:?}");
    assert!(
        !diagnostic.contains(PAYLOAD),
        "Debug leaked a host or vendor payload: {diagnostic}"
    );
}

#[test]
fn configuration_carriers_report_shape_without_vendor_values() {
    let option = ConfigurationOption::new(
        ConfigurationOptionId::new(PAYLOAD),
        ConfigurationCategory::Other(String::from(PAYLOAD)),
        ConfigurationValueType::Other(String::from(PAYLOAD)),
    )
    .with_name(PAYLOAD)
    .with_description(PAYLOAD)
    .with_current(ConfigurationValue::text(PAYLOAD))
    .with_values(vec![
        ConfigurationOptionValue::new(ConfigurationValue::text(PAYLOAD))
            .with_display_name(PAYLOAD)
            .with_description(PAYLOAD),
    ]);
    let configuration = Configuration::unknown()
        .with_model(PAYLOAD)
        .with_effort(PAYLOAD)
        .with_native(
            ConfigurationOptionId::new(PAYLOAD),
            ConfigurationValue::text(PAYLOAD),
        );
    let patch = ConfigurationPatch::new()
        .model(ConfigurationChange::Set(String::from(PAYLOAD)))
        .native(
            ConfigurationOptionId::new(PAYLOAD),
            ConfigurationChange::Set(ConfigurationValue::text(PAYLOAD)),
        );
    let state = ConfigurationState::new(
        configuration.clone(),
        configuration.clone(),
        configuration.clone(),
    );
    let outcome =
        ConfigurationOutcome::applied(state.clone(), vec![ConfigurationOptionId::new(PAYLOAD)])
            .rejecting(
                vec![RejectedSetting::new(
                    ConfigurationOptionId::new(PAYLOAD),
                    SettingRejection::RefusedByVendor {
                        detail: String::from(PAYLOAD),
                    },
                )],
                Rollback::Failed,
            );

    assert_payload_free(configuration);
    assert_payload_free(patch);
    assert_payload_free(state);
    assert_payload_free(ConfigurationOptionId::new(PAYLOAD));
    assert_payload_free(ConfigurationValue::text(PAYLOAD));
    assert_payload_free(ConfigurationCategory::Other(String::from(PAYLOAD)));
    assert_payload_free(ConfigurationValueType::Other(String::from(PAYLOAD)));
    assert_payload_free(option.clone());
    assert_payload_free(ConfigurationCatalog::new(vec![option]));
    assert_payload_free(outcome);
}

#[test]
fn activity_content_extensions_and_operation_reference_do_not_replay_payloads() {
    let plan = ActivityContent::Plan {
        steps: vec![PlanStep::new(PAYLOAD).with_id(PAYLOAD)],
    };
    let diff = ActivityContent::Diff {
        files: vec![
            FileChange::new(PAYLOAD, FileChangeKind::Modified)
                .moved_from(PAYLOAD)
                .with_unified_diff(PAYLOAD),
        ],
    };
    let output = ActivityContent::Output {
        text: String::from(PAYLOAD),
    };
    let extensions = Extensions::new().with(PAYLOAD, ExtensionValue::text(PAYLOAD));
    let operation = OperationRef::new(
        SessionId::new(PAYLOAD),
        TurnId::new(PAYLOAD),
        AttemptId::FIRST,
    );

    assert_payload_free(plan);
    assert_payload_free(diff);
    assert_payload_free(output);
    assert_payload_free(PlanStep::new(PAYLOAD).with_id(PAYLOAD));
    assert_payload_free(
        FileChange::new(PAYLOAD, FileChangeKind::Modified)
            .moved_from(PAYLOAD)
            .with_unified_diff(PAYLOAD),
    );
    assert_payload_free(ExtensionValue::text(PAYLOAD));
    assert_payload_free(extensions);
    assert_payload_free(operation);
}

#[test]
fn snapshot_and_identity_keep_payloads_out_of_aggregate_diagnostics() {
    let identity = HarnessIdentity::custom(PAYLOAD, PAYLOAD, Some(PAYLOAD))
        .expect("expected a valid identity");
    let snapshot = SessionSnapshot::opening(
        SessionIds {
            session_id: SessionId::new(PAYLOAD),
            native_session_id: String::from(PAYLOAD),
        },
        identity.clone(),
        TransportSelection::new(Some(TransportKind::Stdio), TransportKind::Stdio),
        SystemTime::UNIX_EPOCH,
    )
    .with_fallback_reason(PAYLOAD);

    assert_payload_free(identity);
    assert_payload_free(snapshot);
}
