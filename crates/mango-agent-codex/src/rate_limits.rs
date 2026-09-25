//! The account's plan quota, as the core models it.
//!
//! Codex reports two named windows with a percentage the server already computed, which is the
//! easy case: no division here, so no `NaN` to clamp. What does need care is absence — a window
//! the server did not send is unknown, and a snapshot that invented a zero for it would render as
//! "none of your quota is used" on a reading nobody took.

use std::time::{Duration, SystemTime};

use mango_external_agents::event::{
    AccountLimits, Credits, RateLimitWindow, ResetCredit, ResetCredits, SpendControl,
};

use crate::protocol::notifications::{
    CreditsSnapshot, RateLimitResetCreditsSummary, RateLimitSnapshot,
    RateLimitWindow as VendorRateLimitWindow,
};

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

    let mut limits = AccountLimits::unknown(observed_at);
    limits.windows = windows;
    limits.plan_type = snapshot.plan_type.clone();
    limits.credits = snapshot.credits.as_ref().map(to_credits);
    limits.spend_control = to_spend_control(snapshot);
    limits
}

/// The earned rate-limit resets a full `account/rateLimits/read` reported, as the core models
/// them. A negative count is not a count and reads as zero; timestamps are Unix seconds.
///
/// # Example
///
/// ```
/// use mango_agent_codex::protocol::RateLimitResetCreditsSummary;
/// use mango_agent_codex::rate_limits::to_reset_credits;
///
/// let summary: RateLimitResetCreditsSummary =
///     serde_json::from_value(serde_json::json!({"availableCount": 2, "credits": null})).unwrap();
/// let resets = to_reset_credits(&summary);
/// assert_eq!(resets.available_count, 2);
/// assert!(resets.credits.is_none());
/// ```
#[must_use]
pub fn to_reset_credits(summary: &RateLimitResetCreditsSummary) -> ResetCredits {
    let mut resets = ResetCredits::new(u64::try_from(summary.available_count).unwrap_or(0));
    resets.credits = summary.credits.as_ref().map(|credits| {
        credits
            .iter()
            .map(|credit| {
                let mut row = ResetCredit::new(credit.id.clone(), credit.status.clone());
                row.reset_type = credit.reset_type.clone().filter(|kind| !kind.is_empty());
                row.granted_at = credit.granted_at.and_then(unix_seconds);
                row.expires_at = credit.expires_at.and_then(unix_seconds);
                row.title = credit.title.clone().filter(|title| !title.is_empty());
                row.description = credit
                    .description
                    .clone()
                    .filter(|description| !description.is_empty());
                row
            })
            .collect()
    });
    resets
}

fn to_credits(credits: &CreditsSnapshot) -> Credits {
    let mut mapped = Credits::default();
    mapped.has_credits = Some(credits.has_credits);
    mapped.unlimited = Some(credits.unlimited);
    mapped.balance = credits
        .balance
        .clone()
        .filter(|balance| !balance.is_empty());
    mapped
}

/// The spend-control state, or nothing when the server reported neither a limit nor whether it
/// is reached: `null` there is unavailable, not a recovery.
fn to_spend_control(snapshot: &RateLimitSnapshot) -> Option<SpendControl> {
    if snapshot.individual_limit.is_none() && snapshot.spend_control_reached.is_none() {
        return None;
    }
    let mut spend = SpendControl::default();
    if let Some(limit) = &snapshot.individual_limit {
        spend.limit = Some(limit.limit.clone()).filter(|text| !text.is_empty());
        spend.used = Some(limit.used.clone()).filter(|text| !text.is_empty());
        spend.remaining_percent = limit.remaining_percent;
        spend.resets_at = limit.resets_at.and_then(unix_seconds);
    }
    spend.reached = snapshot.spend_control_reached;
    Some(spend)
}

/// A sparse `account/rateLimits/updated` snapshot, laid over the last full reading.
///
/// The update may carry only what changed. A window, plan, credit snapshot, spend-control limit
/// or reached flag it leaves out, or sends as `null`, keeps the baseline's value — an explicit `null` is not a reading that the window went away —
/// and a present value overwrites it. Inside a present window, `usedPercent` always overwrites
/// (the vendor declares it required) while a `null` duration or reset time keeps the baseline's.
///
/// # Example
///
/// ```
/// use mango_agent_codex::protocol::RateLimitSnapshot;
/// use mango_agent_codex::rate_limits::merge;
///
/// let baseline: RateLimitSnapshot = serde_json::from_value(serde_json::json!({
///     "primary": {"usedPercent": 10.0}, "secondary": {"usedPercent": 20.0}, "planType": "plus"
/// })).unwrap();
/// let update: RateLimitSnapshot = serde_json::from_value(serde_json::json!({
///     "primary": {"usedPercent": 30.0}, "secondary": null
/// })).unwrap();
/// let merged = merge(&baseline, &update);
/// assert_eq!(merged.primary.map(|window| window.used_percent), Some(30.0));
/// assert_eq!(merged.secondary.map(|window| window.used_percent), Some(20.0));
/// assert_eq!(merged.plan_type.as_deref(), Some("plus"));
/// ```
#[must_use]
pub fn merge(baseline: &RateLimitSnapshot, update: &RateLimitSnapshot) -> RateLimitSnapshot {
    RateLimitSnapshot {
        primary: merge_window(baseline.primary, update.primary),
        secondary: merge_window(baseline.secondary, update.secondary),
        plan_type: update
            .plan_type
            .clone()
            .or_else(|| baseline.plan_type.clone()),
        credits: merge_credits(baseline.credits.as_ref(), update.credits.as_ref()),
        individual_limit: update
            .individual_limit
            .clone()
            .or_else(|| baseline.individual_limit.clone()),
        spend_control_reached: update
            .spend_control_reached
            .or(baseline.spend_control_reached),
    }
}

/// Credits the update sent overwrite; a `null` balance keeps the baseline's.
fn merge_credits(
    baseline: Option<&CreditsSnapshot>,
    update: Option<&CreditsSnapshot>,
) -> Option<CreditsSnapshot> {
    let Some(update) = update else {
        return baseline.cloned();
    };
    let mut merged = update.clone();
    if merged.balance.is_none() {
        merged.balance = baseline.and_then(|baseline| baseline.balance.clone());
    }
    Some(merged)
}

fn merge_window(
    baseline: Option<VendorRateLimitWindow>,
    update: Option<VendorRateLimitWindow>,
) -> Option<VendorRateLimitWindow> {
    let Some(update) = update else {
        return baseline;
    };
    let Some(baseline) = baseline else {
        return Some(update);
    };
    Some(VendorRateLimitWindow {
        used_percent: update.used_percent,
        window_duration_mins: update
            .window_duration_mins
            .or(baseline.window_duration_mins),
        resets_at: update.resets_at.or(baseline.resets_at),
    })
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

    #[test]
    fn a_present_window_overwrites_while_an_absent_or_null_one_keeps_the_baseline() {
        let baseline = snapshot(serde_json::json!({
            "primary": {"usedPercent": 10.0, "windowDurationMins": 300, "resetsAt": 1000},
            "secondary": {"usedPercent": 20.0, "windowDurationMins": 10080, "resetsAt": 2000},
            "planType": "plus"
        }));
        let merged = super::merge(
            &baseline,
            &snapshot(serde_json::json!({
                "primary": {"usedPercent": 30.0, "windowDurationMins": null, "resetsAt": 1500},
                "secondary": null
            })),
        );
        let primary = merged.primary.expect("expected the primary window");
        assert_eq!(primary.used_percent, 30.0);
        assert_eq!(primary.window_duration_mins, Some(300));
        assert_eq!(primary.resets_at, Some(1500));
        assert_eq!(merged.secondary, baseline.secondary);
        assert_eq!(merged.plan_type.as_deref(), Some("plus"));
    }

    /// The recorded read's shape: credits and a reached flag, no individual limit.
    #[test]
    fn credits_and_spend_control_map_with_absence_kept_absent() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "credits": {"balance": "12.5", "hasCredits": true, "unlimited": false},
                "individualLimit": null, "spendControlReached": false
            })),
            observed_at(),
        );
        let credits = limits.credits.expect("expected credits");
        assert_eq!(credits.balance.as_deref(), Some("12.5"));
        assert_eq!(credits.has_credits, Some(true));
        let spend = limits.spend_control.expect("expected spend control");
        assert_eq!((spend.reached, spend.limit), (Some(false), None));

        let silent = to_account_limits(&snapshot(serde_json::json!({})), observed_at());
        assert!(silent.credits.is_none() && silent.spend_control.is_none());
    }

    #[test]
    fn an_individual_limit_carries_its_amounts_and_reset_time() {
        let limits = to_account_limits(
            &snapshot(serde_json::json!({
                "individualLimit": {"limit": "100", "used": "40", "remainingPercent": 60.0,
                                    "resetsAt": 1789301053},
                "spendControlReached": null
            })),
            observed_at(),
        );
        let spend = limits.spend_control.expect("expected spend control");
        assert_eq!(spend.limit.as_deref(), Some("100"));
        assert_eq!(spend.used.as_deref(), Some("40"));
        assert_eq!(spend.remaining_percent, Some(60.0));
        assert_eq!(
            spend.resets_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_301_053))
        );
        assert_eq!(spend.reached, None, "expected null to stay unavailable");
    }

    /// Sparse updates: credits, the limit and the reached flag follow the same rules as windows.
    #[test]
    fn credits_and_spend_control_merge_like_every_other_field() {
        let baseline = snapshot(serde_json::json!({
            "credits": {"balance": "12.5", "hasCredits": true, "unlimited": false},
            "individualLimit": {"limit": "100", "used": "40", "remainingPercent": 60.0,
                                "resetsAt": 1000},
            "spendControlReached": false
        }));
        let kept = super::merge(
            &baseline,
            &snapshot(serde_json::json!({
                "credits": null, "individualLimit": null, "spendControlReached": null
            })),
        );
        assert_eq!(kept.credits, baseline.credits);
        assert_eq!(kept.individual_limit, baseline.individual_limit);
        assert_eq!(kept.spend_control_reached, Some(false));

        let moved = super::merge(
            &baseline,
            &snapshot(serde_json::json!({
                "credits": {"balance": null, "hasCredits": false, "unlimited": false},
                "spendControlReached": true
            })),
        );
        let credits = moved.credits.expect("expected credits");
        assert!(
            !credits.has_credits,
            "expected the update's flag to overwrite"
        );
        assert_eq!(
            credits.balance.as_deref(),
            Some("12.5"),
            "expected a null balance to keep the baseline's"
        );
        assert_eq!(moved.spend_control_reached, Some(true));
        assert_eq!(moved.individual_limit, baseline.individual_limit);
    }

    #[test]
    fn reset_credits_keep_the_count_authoritative_and_the_rows_as_reported() {
        let summary: crate::protocol::notifications::RateLimitResetCreditsSummary =
            serde_json::from_value(serde_json::json!({"availableCount": 5, "credits": [
                {"id": "r-1", "resetType": "codexRateLimits", "status": "available",
                 "grantedAt": 1787352411, "expiresAt": null, "title": null, "description": ""}
            ]}))
            .expect("expected a summary");
        let resets = super::to_reset_credits(&summary);
        assert_eq!(resets.available_count, 5);
        let rows = resets.credits.expect("expected rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].expires_at, None);
        assert_eq!(
            rows[0].description, None,
            "expected empty text to read as absent"
        );

        let negative: crate::protocol::notifications::RateLimitResetCreditsSummary =
            serde_json::from_value(serde_json::json!({"availableCount": -3}))
                .expect("expected a summary");
        assert_eq!(super::to_reset_credits(&negative).available_count, 0);
    }

    #[test]
    fn a_window_the_baseline_lacked_is_taken_whole_from_the_update() {
        let merged = super::merge(
            &snapshot(serde_json::json!({"primary": {"usedPercent": 10.0}})),
            &snapshot(serde_json::json!({"secondary": {"usedPercent": 5.0}, "planType": "pro"})),
        );
        assert_eq!(merged.primary.map(|window| window.used_percent), Some(10.0));
        assert_eq!(
            merged.secondary.map(|window| window.used_percent),
            Some(5.0)
        );
        assert_eq!(merged.plan_type.as_deref(), Some("pro"));
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
