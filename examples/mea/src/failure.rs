//! Shared operation/cleanup precedence and operator recovery diagnostics.

use mango_external_agents::{Error, Result};

/// The note an operator needs when process cleanup is incomplete, or an empty string when it is
/// not: `; process-tree cleanup is unconfirmed ...`, ready to follow the error's own text.
///
/// The typed recovery control is still owned here. The CLI cannot hand a Rust control to its
/// operator; it reports the launched pid for manual reconciliation rather than blindly repeating
/// the control's one-stop request. This is a suffix, not a formatter: `command_failure` is the one
/// place that turns an error into the line an operator reads, because a formatter that prints the
/// error alone drops the vendor's message.
pub(super) fn cleanup_notes(error: &Error) -> String {
    match error.cleanup_control().map(|control| control.pid()) {
        Some(Some(pid)) => format!(
            "; process-tree cleanup is unconfirmed for launched pid {pid}; inspect the process before retrying"
        ),
        Some(None) => String::from(
            "; process-tree cleanup is unconfirmed and the injected control has no pid; inspect the launcher before retrying",
        ),
        None => String::new(),
    }
}

/// Retains the cleanup failure unchanged, reporting the primary context separately when both fail.
pub(crate) fn with_cleanup<T>(
    outcome: Result<T>,
    closed: Result<()>,
    report_primary: impl FnOnce(&Error),
) -> Result<T> {
    match (outcome, closed) {
        (Err(primary), Err(cleanup)) => {
            report_primary(&primary);
            Err(cleanup)
        }
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (outcome, Ok(())) => outcome,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mango_external_agents::{CancelReason, ExitStatus, ProcessControl};

    use super::*;

    /// A recovery control with a chosen pid and nothing to stop.
    struct FakeRecoveryControl(Option<u32>);

    #[async_trait::async_trait]
    impl ProcessControl for FakeRecoveryControl {
        fn pid(&self) -> Option<u32> {
            self.0
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
        async fn wait(&self) -> Result<ExitStatus> {
            Ok(ExitStatus::default())
        }
        async fn kill(&self, _reason: CancelReason) -> Result<()> {
            Ok(())
        }
    }

    fn unconfirmed_cleanup(pid: Option<u32>) -> Error {
        Error::CleanupRequired {
            control: Arc::new(FakeRecoveryControl(pid)),
            source: Box::new(Error::Closed { subject: "cleanup" }),
        }
    }

    #[test]
    fn an_error_without_a_cleanup_control_has_no_note() {
        let error = Error::Closed { subject: "cleanup" };

        assert_eq!(cleanup_notes(&error), "");
    }

    #[test]
    fn an_unconfirmed_cleanup_names_the_launched_pid() {
        let notes = cleanup_notes(&unconfirmed_cleanup(Some(4242)));

        assert_eq!(
            notes,
            "; process-tree cleanup is unconfirmed for launched pid 4242; inspect the process before retrying"
        );
    }

    #[test]
    fn an_unconfirmed_cleanup_without_a_pid_does_not_invent_one() {
        let notes = cleanup_notes(&unconfirmed_cleanup(None));

        assert_eq!(
            notes,
            "; process-tree cleanup is unconfirmed and the injected control has no pid; inspect the launcher before retrying"
        );
    }
}
