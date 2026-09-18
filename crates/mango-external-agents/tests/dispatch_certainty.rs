//! Dispatch certainty belongs to the operation boundary, independently of failure category.

use mango_external_agents::{Dispatch, Error};

#[test]
fn an_error_without_an_operation_stage_does_not_claim_acceptance() {
    let errors = [
        Error::Protocol {
            expected: String::from("a supported local configuration"),
            received: String::from("an unsupported local configuration"),
        },
        Error::HostConfiguration {
            expected: "a supported configuration",
            received: String::from("an unsupported configuration"),
        },
    ];
    for error in errors {
        assert_eq!(
            error.dispatch(),
            Dispatch::AcceptanceUnknown,
            "an error category does not establish the operation stage: {error}"
        );
    }
}

#[test]
fn operation_annotations_preserve_the_cause_and_replace_previous_certainty() {
    use mango_external_agents::{ErrorCode, VendorError};
    use std::error::Error as _;

    let original = Error::Vendor(
        VendorError::new(ErrorCode::from_static("retryable"), "Bearer payload-secret")
            .with_vendor_code("busy", true),
    );
    let display = original.to_string();
    let error = original
        .with_dispatch(Dispatch::NotSubmitted)
        .with_dispatch(Dispatch::Accepted);
    assert_eq!(error.dispatch(), Dispatch::Accepted);
    assert!(error.retryable());
    assert_eq!(error.to_string(), display);
    assert!(!format!("{error:?}").contains("payload-secret"));
    assert!(matches!(error.cause(), Error::Vendor(_)));
    let source = error.source().expect("expected original typed failure");
    assert!(matches!(
        source.downcast_ref::<Error>(),
        Some(Error::Vendor(_))
    ));
}

#[test]
fn the_same_protocol_failure_can_be_local_or_after_acceptance() {
    for dispatch in [
        Dispatch::NotSubmitted,
        Dispatch::Accepted,
        Dispatch::AcceptanceUnknown,
    ] {
        let error = Error::Protocol {
            expected: String::from("a valid configuration"),
            received: String::from("invalid"),
        }
        .with_dispatch(dispatch);
        assert_eq!(error.dispatch(), dispatch);
        assert_eq!(
            error.dispatch().is_safe_to_replay(),
            dispatch == Dispatch::NotSubmitted
        );
        assert!(!error.retryable());
    }
}
