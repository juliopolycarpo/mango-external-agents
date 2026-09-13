//! The account's plan quota, as the core models it.
//!
//! Codex reports two named windows with a percentage the server already computed, which is the
//! easy case: no division here, so no `NaN` to clamp. What does need care is absence — a window
//! the server did not send is unknown, and a snapshot that invented a zero for it would render as
//! "none of your quota is used" on a reading nobody took.

use std::time::{Duration, SystemTime};

use mango_external_agents::event::{AccountLimits, RateLimitWindow};

use crate::protocol::notifications::{RateLimitSnapshot, RateLimitWindow as VendorRateLimitWindow};

/// The vendor's own label for the shorter window.
const PRIMARY_LABEL: &str = "primary";
/// The vendor's own label for the longer one.
const SECONDARY_LABEL: &str = "secondary";

/// One snapshot, as the core's [`AccountLimits`].
///
/// # Example
///
/// ```
/// use mango_agent_codex::rate_limits::to_account_limits;
/// use mango_agent_codex::protocol::RateLimitSnapshot;
/// use std::time::SystemTime;
///
/// let limits = to_account_limits(&RateLimitSnapshot::default(), SystemTime::UNIX_EPOCH);
/// assert!(limits.windows.is_empty());
/// ```
#[must_use]
pub fn to_account_limits(snapshot: &RateLimitSnapshot, observed_at: SystemTime) -> AccountLimits {
    let windows = [
        (PRIMARY_LABEL, snapshot.primary.as_ref()),
        (SECONDARY_LABEL, snapshot.secondary.as_ref()),
    ]
    .into_iter()
    .filter_map(|(label, window)| window.map(|window| to_window(label, window)))
    .collect();

    AccountLimits {
        windows,
        plan_type: snapshot.plan_type.clone(),
        observed_at,
    }
}

fn to_window(label: &str, window: &VendorRateLimitWindow) -> RateLimitWindow {
    RateLimitWindow {
        label: Some(String::from(label)),
        used_percent: window.used_percent,
        window_duration_minutes: window.window_duration_mins,
        resets_at: window.resets_at.and_then(unix_seconds),
    }
}

/// A Unix timestamp as an instant, or nothing when it names one the clock cannot hold.
///
/// The server writes seconds since the epoch as a signed number. A negative one predates the
/// epoch, which is not a reset time; an absurd one would still convert. Both read as "the vendor
/// did not say", which is what an unusable reading is.
fn unix_seconds(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::to_account_limits;
    use crate::protocol::notifications::RateLimitSnapshot;
    use std::time::{Duration, SystemTime};

    fn observed_at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_283_381)
    }

    fn snapshot(raw: serde_json::Value) -> RateLimitSnapshot {
        serde_json::from_value(raw).expect("expected a snapshot")
    }

    /// The captured frame from a real session, both windows and the plan.
    #[test]
    fn both_windows_travel_with_their_labels_and_their_reset_times() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "limitId": "codex", "limitName": null,
                "primary": {"usedPercent": 4.0, "windowDurationMins": 300, "resetsAt": 1789301053},
                "secondary": {"usedPercent": 6.0, "windowDurationMins": 10080,
                              "resetsAt": 1789817658},
                "planType": "plus"
            })),
            observed_at(),
        );

        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
        assert_eq!(limits.windows.len(), 2);
        assert_eq!(limits.windows[0].label.as_deref(), Some("primary"));
        assert_eq!(limits.windows[0].used_percent, 4.0);
        assert_eq!(limits.windows[0].window_duration_minutes, Some(300));
        assert_eq!(
            limits.windows[0].resets_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_301_053))
        );
        assert_eq!(limits.windows[1].label.as_deref(), Some("secondary"));
        assert_eq!(limits.observed_at, observed_at());
    }

    /// A window the server did not send is a window nobody read. Inventing a zero for it renders
    /// as "none of your quota is used", which is a claim rather than an absence.
    #[test]
    fn a_window_the_server_did_not_send_is_absent_rather_than_zero() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "primary": {"usedPercent": 71.5, "windowDurationMins": 300, "resetsAt": null},
                "secondary": null
            })),
            observed_at(),
        );

        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].used_percent, 71.5);
        assert_eq!(limits.windows[0].resets_at, None);
        assert_eq!(limits.plan_type, None);
    }

    #[test]
    fn a_snapshot_with_nothing_in_it_reports_nothing() {
        let limits = to_account_limits(&RateLimitSnapshot::default(), observed_at());
        assert!(limits.windows.is_empty());
        assert_eq!(limits.plan_type, None);
        assert_eq!(limits.observed_at, observed_at());
    }

    /// A reset time the clock cannot hold is a reading nobody took, not an instant before 1970.
    #[test]
    fn a_reset_time_that_is_not_an_instant_reads_as_unsaid() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "primary": {"usedPercent": 1.0, "resetsAt": -1}
            })),
            observed_at(),
        );
        assert_eq!(limits.windows[0].resets_at, None);
    }

    /// Every percentage the core writes has to be JSON, and the core clamps on the way out. This
    /// proves a nonsense reading survives the trip rather than losing the whole event.
    #[test]
    fn a_percentage_outside_the_scale_is_brought_back_into_it_rather_than_refused() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "primary": {"usedPercent": 4000.0}, "secondary": {"usedPercent": -12.0}
            })),
            observed_at(),
        )
        .normalized();

        assert_eq!(limits.windows[0].used_percent, 100.0);
        assert_eq!(limits.windows[1].used_percent, 0.0);
        serde_json::to_string(&limits).expect("expected a snapshot that can be written as JSON");
    }
}
