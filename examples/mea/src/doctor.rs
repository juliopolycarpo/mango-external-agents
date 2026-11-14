//! Stable, credential-free diagnostic output for every registered harness.
//!
//! A failed probe's `error` is the line `command_failure` built: already read for credentials and
//! bounded, with a marker when it was cut. Neither output reads or bounds it again, since a second
//! cut would drop that marker and the vendor code and request id after it.

use mango_external_agents::{AuthState, Discovery, GateVerdict};
use serde_json::{Value, json};

use crate::DiscoveryReport;

/// Renders all probes as one JSON document. Example: `mea doctor --json`.
pub(crate) fn json(reports: &[DiscoveryReport]) -> Value {
    Value::Array(reports.iter().map(row).collect())
}

fn row(report: &DiscoveryReport) -> Value {
    let discovery = match &report.result {
        Ok(discovery) => discovery,
        Err(error) => {
            return json!({"harness": report.kind.to_string(), "installed": null, "version": null, "gate": "unknown", "auth": "unknown", "error": error});
        }
    };
    let (auth, login_hint) = account(discovery);
    json!({
        "harness": report.kind.to_string(),
        "installed": installed(discovery),
        "version": discovery.version,
        "gate": match discovery.gate {
            GateVerdict::Usable => "usable",
            GateVerdict::NotInstalled => "not-installed",
            GateVerdict::VersionTooOld { .. } => "version-too-old",
            GateVerdict::MissingRequiredSurface { .. } => "missing-required-surface",
            _ => "unknown",
        },
        "gateDetail": format!("{:?}", discovery.gate),
        "auth": auth,
        "loginHint": login_hint,
        "capabilities": discovery.capabilities,
        "models": discovery.models,
        "permissionMatrix": discovery.permission_matrix,
    })
}

/// The account state and the vendor's own login command, when it named one.
fn account(discovery: &Discovery) -> (&'static str, Option<&str>) {
    match &discovery.auth {
        AuthState::LoggedIn { .. } => ("logged-in", None),
        AuthState::LoggedOut { login_hint } => ("logged-out", Some(login_hint.as_str())),
        _ => ("unknown", None),
    }
}

fn installed(discovery: &Discovery) -> Option<bool> {
    match discovery.gate {
        GateVerdict::NotInstalled => Some(false),
        GateVerdict::Usable
        | GateVerdict::VersionTooOld { .. }
        | GateVerdict::MissingRequiredSurface { .. } => Some(true),
        _ if discovery.executable.is_some() || discovery.version.is_some() => Some(true),
        _ => None,
    }
}

/// Renders one probe for a terminal. Example: `mea doctor --harness claude`.
///
/// Reads the discovery directly rather than through [`row`]: a terminal line needs five scalars,
/// and building the JSON document would serialise this harness's capabilities, model catalog and
/// six-cell permission matrix only to drop them.
pub(crate) fn text(report: &DiscoveryReport) -> String {
    let discovery = match &report.result {
        Ok(discovery) => discovery,
        Err(error) => {
            return format!(
                "{}: installed=null version=unknown gate=unknown auth=unknown; {}",
                report.kind, error
            );
        }
    };
    let (auth, login_hint) = account(discovery);
    let mut line = format!(
        "{}: installed={} version={} gate={:?} auth={auth}",
        report.kind,
        installed(discovery).map_or_else(|| String::from("null"), |found| found.to_string()),
        discovery.version.as_deref().unwrap_or("unknown"),
        discovery.gate,
    );
    if let Some(hint) = login_hint {
        line.push_str(&format!("; run `{hint}`"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use mango_external_agents::HarnessId;
    use mango_external_agents::normalize::TextLimit;

    /// A probe failure is the CLI's own message, not a stderr line: a bare token in it is read for
    /// the well-known prefixes where the failure line is built, and neither output prints it.
    #[test]
    fn a_bare_token_in_a_probe_failure_is_redacted_in_text_and_json() {
        let token = format!(
            "{}0123456789abcdefghijklmnopqrstuvwxyz",
            concat!("gh", "p_")
        );
        let report = probe_report_failing_with(&format!("probe failed for {token}"));
        let (shown, as_json) = (text(&report), row(&report).to_string());
        assert!(
            !shown.contains(&token) && !as_json.contains(&token),
            "expected the token out of both outputs | received: {shown:?} and {as_json:?}"
        );
    }

    /// A probe report as `discover_with` builds it for a vendor failure: the same line a `mea
    /// turn` failure ends with, vendor code and request id included.
    fn probe_report_failing_with(message: &str) -> DiscoveryReport {
        let failure = mango_external_agents::Error::Vendor(
            mango_external_agents::VendorError::new(
                mango_external_agents::ErrorCode::from_static("probe-failed"),
                message,
            )
            .with_vendor_code("-32603", true)
            .with_request_id("req-7"),
        );
        DiscoveryReport {
            kind: HarnessId::codex(),
            result: Err(crate::command_failure(&failure)),
        }
    }

    /// The error as each output shows it, for an assertion that must hold in both. The text line
    /// puts the error after the harness's own `; ` separator.
    fn shown_in_both(report: &DiscoveryReport) -> [(&'static str, String); 2] {
        let line = text(report);
        let (_, error) = line
            .split_once("; ")
            .expect("expected the text line to separate the error with `; `");
        [
            ("text", error.to_owned()),
            (
                "json",
                row(report)["error"]
                    .as_str()
                    .expect("expected an error string in the json row")
                    .to_owned(),
            ),
        ]
    }

    /// A message cut by the failure summary says so, and the doctor line keeps the marker with the
    /// vendor code and request id after it.
    #[test]
    fn a_cut_probe_message_keeps_its_marker_and_the_vendor_ids() {
        let limit = TextLimit::ErrorMessage.max_code_points();
        let report = probe_report_failing_with(&"x".repeat(limit + 500));
        for (output, shown) in shown_in_both(&report) {
            assert!(
                shown.contains("[truncated: first 2048 characters of the message shown]")
                    && shown.ends_with("(vendor code -32603) (request req-7)"),
                "({output}): expected the marker and both ids | received: {shown:?}"
            );
        }
    }

    /// A message just under the limit is not cut by the summary, so the doctor line may not cut
    /// it either: the code's prefix and the vendor ids come on top of the 2048 characters.
    #[test]
    fn a_near_limit_probe_message_comes_through_whole() {
        let limit = TextLimit::ErrorMessage.max_code_points();
        let message = "y".repeat(limit - 5);
        let report = probe_report_failing_with(&message);
        for (output, shown) in shown_in_both(&report) {
            assert_eq!(
                shown,
                format!(
                    "probe-failed: vendor failure: {message} (vendor code -32603) (request req-7)"
                ),
                "({output}): expected the whole line, received {} characters",
                shown.chars().count()
            );
        }
    }

    /// Escaping makes the message longer than the characters the vendor wrote: 750 lines of "a" are
    /// 1500 characters as written and 2250 on the line, and neither count is a reason to cut it again.
    #[test]
    fn an_escaped_probe_message_is_not_cut_a_second_time() {
        let report = probe_report_failing_with(&format!("{}end", "a\n".repeat(750)));
        for (output, shown) in shown_in_both(&report) {
            assert!(
                shown.contains(&format!("{}end", "a\\n".repeat(750)))
                    && shown.ends_with("(request req-7)"),
                "({output}): expected 750 escaped lines and the request id | received {} characters",
                shown.chars().count()
            );
        }
    }

    #[test]
    fn logged_out_reports_the_vendors_login_command() {
        let report = DiscoveryReport {
            kind: HarnessId::claude(),
            result: Ok(Discovery {
                gate: GateVerdict::Usable,
                version: Some("2.1.0".into()),
                auth: AuthState::LoggedOut {
                    login_hint: "claude auth login".into(),
                },
                ..Discovery::not_installed()
            }),
        };
        assert!(text(&report).contains("run `claude auth login`"));
        let output = json(&[report]);
        assert_eq!(output[0]["installed"], true);
        assert_eq!(output[0]["auth"], "logged-out");
        assert_eq!(output[0]["version"], "2.1.0");
    }

    #[test]
    fn missing_unknown_and_failed_probes_remain_distinct() {
        assert_eq!(installed(&Discovery::not_installed()), Some(false));
        assert_eq!(
            installed(&Discovery {
                gate: GateVerdict::Unknown,
                ..Discovery::not_installed()
            }),
            None
        );
        let report = DiscoveryReport {
            kind: HarnessId::codex(),
            result: Err("probe failed".into()),
        };
        assert!(text(&report).contains("probe failed"));
        assert_eq!(row(&report)["installed"], Value::Null);
        assert_eq!(row(&report)["auth"], "unknown");
    }

    #[test]
    fn a_missing_required_surface_is_an_installed_but_gated_cli() {
        let report = DiscoveryReport {
            kind: HarnessId::claude(),
            result: Ok(Discovery {
                gate: GateVerdict::MissingRequiredSurface {
                    expected: "a required launch flag",
                    received: "the flag was absent from help",
                },
                ..Discovery::not_installed()
            }),
        };
        let output = json(&[report]);
        assert_eq!(output[0]["gate"], "missing-required-surface");
        assert_eq!(output[0]["installed"], true);
    }
}
