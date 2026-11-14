//! Stable, credential-free diagnostic output for every registered harness.

use mango_external_agents::normalize::TextLimit;
use mango_external_agents::{AuthState, Discovery, GateVerdict, HarnessId};
use serde_json::{Value, json};

use crate::DiscoveryReport;

/// Renders all probes as one JSON document. Example: `mea doctor --json`.
pub(crate) fn json(reports: &[DiscoveryReport]) -> Value {
    Value::Array(reports.iter().map(row).collect())
}

/// A probe failure as it is printed: the CLI's own message, read for names and for the well-known
/// token prefixes, and bounded.
fn shown_error(error: &str) -> String {
    mango_external_agents::redact::bounded(error, TextLimit::ErrorMessage).text
}

/// What `mea doctor --transport unix-socket` learned by attaching: the server's thread, or the
/// refusal the harness gave, in its own words.
pub(crate) struct AttachReport {
    pub kind: HarnessId,
    pub result: Result<String, String>,
}

/// [`json()`], with the attach result under `attach` on the row of the harness that was attached.
/// Example: `mea doctor --harness codex --transport unix-socket --socket PATH --json`.
pub(crate) fn json_with_attach(
    reports: &[DiscoveryReport],
    attach: Option<&AttachReport>,
) -> Value {
    let mut rows = json(reports);
    let Some(attach) = attach else {
        return rows;
    };
    let outcome = match &attach.result {
        Ok(thread) => json!({"transport": "unix-socket", "attached": true, "thread": thread}),
        Err(error) => {
            json!({"transport": "unix-socket", "attached": false, "error": shown_error(error)})
        }
    };
    let harness = attach.kind.to_string();
    if let Some(row) = rows
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["harness"] == harness))
    {
        row["attach"] = outcome;
    }
    rows
}

/// The attach result as a terminal line. Example: `codex: attach=refused over unix-socket; ...`.
pub(crate) fn attach_text(attach: &AttachReport) -> String {
    match &attach.result {
        Ok(thread) => format!(
            "{}: attach=ok over unix-socket; thread {thread}",
            attach.kind
        ),
        Err(error) => format!(
            "{}: attach=refused over unix-socket; {}",
            attach.kind,
            shown_error(error)
        ),
    }
}

fn row(report: &DiscoveryReport) -> Value {
    let discovery = match &report.result {
        Ok(discovery) => discovery,
        Err(error) => {
            return json!({"harness": report.kind.to_string(), "installed": null, "version": null, "gate": "unknown", "auth": "unknown", "error": shown_error(error)});
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
                report.kind,
                shown_error(error)
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

    /// A probe failure is the CLI's own message, not a stderr line: a bare token in it is read for
    /// the well-known prefixes as well.
    #[test]
    fn a_bare_token_in_a_probe_failure_is_redacted_in_text_and_json() {
        let token = format!(
            "{}0123456789abcdefghijklmnopqrstuvwxyz",
            concat!("gh", "p_")
        );
        let report = DiscoveryReport {
            kind: HarnessId::codex(),
            result: Err(format!("probe failed for {token}")),
        };
        let (shown, as_json) = (text(&report), row(&report).to_string());
        assert!(
            !shown.contains(&token) && !as_json.contains(&token),
            "expected the token out of both outputs | received: {shown:?} and {as_json:?}"
        );
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

    fn probed_codex() -> DiscoveryReport {
        DiscoveryReport {
            kind: HarnessId::codex(),
            result: Ok(Discovery::not_installed()),
        }
    }

    #[test]
    fn an_attached_socket_adds_the_thread_to_the_codex_row_only() {
        let attach = AttachReport {
            kind: HarnessId::codex(),
            result: Ok(String::from("thread-1")),
        };
        let output = json_with_attach(&[probed_codex()], Some(&attach));
        assert_eq!(
            output[0]["attach"],
            json!({"transport": "unix-socket", "attached": true, "thread": "thread-1"})
        );
        assert_eq!(
            attach_text(&attach),
            "codex: attach=ok over unix-socket; thread thread-1"
        );
        assert_eq!(
            json_with_attach(&[probed_codex()], None),
            json(&[probed_codex()]),
            "expected no attach key when no socket was named"
        );
    }

    #[test]
    fn a_refused_attach_carries_the_refusal_and_stays_redacted() {
        let token = format!(
            "{}0123456789abcdefghijklmnopqrstuvwxyz",
            concat!("gh", "p_")
        );
        let attach = AttachReport {
            kind: HarnessId::codex(),
            result: Err(format!("expected a socket, received {token}")),
        };
        let output = json_with_attach(&[probed_codex()], Some(&attach)).to_string();
        let line = attach_text(&attach);
        assert!(
            line.starts_with(
                "codex: attach=refused over unix-socket; expected a socket, received "
            ),
            "{line}"
        );
        assert!(
            !output.contains(&token) && !line.contains(&token),
            "expected the token out of both outputs | received: {line:?} and {output:?}"
        );
        assert!(output.contains("\"attached\":false"), "{output}");
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
