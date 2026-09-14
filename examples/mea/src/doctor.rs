//! Stable, credential-free diagnostic output for every registered harness.

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
            return json!({"harness": report.kind.to_string(), "installed": null, "version": null, "gate": "unknown", "auth": "unknown", "error": mango_external_agents::redact::stderr_text(error)});
        }
    };
    let (auth, login_hint) = match &discovery.auth {
        AuthState::LoggedIn { .. } => ("logged-in", None),
        AuthState::LoggedOut { login_hint } => ("logged-out", Some(login_hint)),
        _ => ("unknown", None),
    };
    json!({
        "harness": report.kind.to_string(),
        "installed": installed(discovery),
        "version": discovery.version,
        "gate": match discovery.gate {
            GateVerdict::Usable => "usable",
            GateVerdict::NotInstalled => "not-installed",
            GateVerdict::VersionTooOld { .. } => "version-too-old",
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

fn installed(discovery: &Discovery) -> Option<bool> {
    match discovery.gate {
        GateVerdict::NotInstalled => Some(false),
        GateVerdict::Usable | GateVerdict::VersionTooOld { .. } => Some(true),
        _ if discovery.executable.is_some() || discovery.version.is_some() => Some(true),
        _ => None,
    }
}

/// Renders one probe for a terminal. Example: `mea doctor --harness claude`.
pub(crate) fn text(report: &DiscoveryReport) -> String {
    let data = row(report);
    let mut line = format!(
        "{}: installed={} version={} gate={} auth={}",
        report.kind,
        data["installed"],
        data["version"].as_str().unwrap_or("unknown"),
        data["gateDetail"].as_str().unwrap_or("unknown"),
        data["auth"].as_str().unwrap_or("unknown")
    );
    if let Some(hint) = data["loginHint"].as_str() {
        line.push_str(&format!("; run `{hint}`"));
    }
    if let Some(error) = data["error"].as_str() {
        line.push_str(&format!("; {error}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use mango_external_agents::HarnessKind;

    #[test]
    fn logged_out_reports_the_vendors_login_command() {
        let report = DiscoveryReport {
            kind: HarnessKind::Claude,
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
            kind: HarnessKind::Codex,
            result: Err("probe failed".into()),
        };
        assert!(text(&report).contains("probe failed"));
        assert_eq!(row(&report)["installed"], Value::Null);
        assert_eq!(row(&report)["auth"], "unknown");
    }
}
