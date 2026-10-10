//! `notice` in, [`EventKind::Notice`] out.
//!
//! A notice is fire-and-forget information for the person at the host: live, not session history,
//! and nothing an agent may rely on being shown. It is vendor-written text for display only.
//!
//! Protocol: <https://agentclientprotocol.com/rfds/session-notices>, stable in v1 from schema 1.11.

use agent_client_protocol::schema::v1::{Notice, NoticeSeverity as WireSeverity};
use mango_external_agents::NoticeSeverity;
use mango_external_agents::event::EventKind;

/// The event one `notice` frame produces, or none when its title has nothing to show.
///
/// The protocol requires a non-empty title that can stand alone. One that is blank, or is only
/// characters the core strips, is dropped here rather than emitted: the core would refuse it, and
/// a refused event fails the turn, which is too much for a banner with no words on it. Nothing is
/// logged for it, as for every other frame this reducer cannot use.
pub(super) fn notice(notice: Notice) -> Vec<EventKind> {
    EventKind::notice(
        severity(notice.severity),
        &notice.title,
        notice.description.as_deref(),
    )
    .into_iter()
    .collect()
}

/// The neutral severity for the wire's, keeping a spelling this build does not know.
fn severity(severity: WireSeverity) -> NoticeSeverity {
    match severity {
        WireSeverity::Info => NoticeSeverity::Info,
        WireSeverity::Warning => NoticeSeverity::Warning,
        WireSeverity::Error => NoticeSeverity::Error,
        WireSeverity::Other(name) => NoticeSeverity::Other(name),
        // `#[non_exhaustive]`: a severity a later schema names and this build has no arm for is
        // still a notice, shown generically under the spelling the schema gives it.
        other => NoticeSeverity::Other(
            serde_json::to_value(&other)
                .ok()
                .and_then(|value| value.as_str().map(String::from))
                .unwrap_or_else(|| String::from("unknown")),
        ),
    }
}

#[cfg(test)]
mod tests;
