//! Shared operation/cleanup precedence and operator recovery diagnostics.

use mango_external_agents::{Error, Result};

/// Names incomplete process cleanup while the typed recovery control is still owned here.
/// The CLI cannot hand a Rust control to its operator; it reports the launched pid for manual
/// reconciliation rather than blindly repeating the control's one-stop request.
pub(crate) fn describe(error: &Error) -> String {
    let cleanup = error.cleanup_control().map(|control| control.pid());
    match cleanup {
        Some(Some(pid)) => format!(
            "{error}; process-tree cleanup is unconfirmed for launched pid {pid}; inspect the process before retrying"
        ),
        Some(None) => format!(
            "{error}; process-tree cleanup is unconfirmed and the injected control has no pid; inspect the launcher before retrying"
        ),
        None => error.to_string(),
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
