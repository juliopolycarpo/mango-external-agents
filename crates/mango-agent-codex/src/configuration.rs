//! Validation and request-scoped app-server configuration.

use std::collections::BTreeMap;

use mango_external_agents::{ConfigurationPatch, Error, McpServer, Result, normalize};
use serde_json::Value;

/// Validates explicit ids without assuming that the vendor's model catalog is closed.
pub(crate) fn validate_ids(patch: &ConfigurationPatch) -> Result<()> {
    for value in [patch.model.set_value(), patch.effort.set_value()]
        .into_iter()
        .flatten()
    {
        if value.trim() != value
            || value.chars().any(char::is_control)
            || normalize::opaque_id(value, "configuration id").is_err()
        {
            return Err(Error::HostConfiguration {
                expected: "a nonempty, bounded model or effort id without controls",
                received: format!("an invalid configuration id of {} bytes", value.len()),
            });
        }
    }
    Ok(())
}

/// Builds one per-thread override for host MCP servers and the opening effort.
pub(crate) fn thread_override(
    patch: &ConfigurationPatch,
    servers: &[McpServer],
) -> Result<Option<BTreeMap<String, Value>>> {
    validate_ids(patch)?;
    let mut config = super::mcp::override_for(servers)?.unwrap_or_default();
    if let Some(effort) = patch.effort.set_value() {
        config.insert(
            String::from("model_reasoning_effort"),
            Value::String(effort.clone()),
        );
    }
    Ok((!config.is_empty()).then_some(config))
}

#[cfg(test)]
mod tests {
    use mango_external_agents::{ConfigurationChange, ConfigurationPatch, McpServer};

    use super::{thread_override, validate_ids};

    #[test]
    fn explicit_ids_keep_opaque_vendor_spelling_but_refuse_control_characters() {
        let opaque = ConfigurationPatch::new()
            .model(ConfigurationChange::Set(String::from("custom-model.v2")));
        assert!(validate_ids(&opaque).is_ok());
        let malformed = ConfigurationPatch::new()
            .effort(ConfigurationChange::Set(String::from("high\nunsafe")));
        assert!(validate_ids(&malformed).is_err());
    }

    #[test]
    fn thread_override_combines_effort_and_mcp_without_changing_either_entry() {
        let patch =
            ConfigurationPatch::new().effort(ConfigurationChange::Set(String::from("ultra")));
        let config = thread_override(&patch, &[McpServer::stdio("docs", "docs-mcp")])
            .expect("expected an override")
            .expect("expected nonempty config");
        assert_eq!(config["model_reasoning_effort"], "ultra");
        assert_eq!(config["mcp_servers"]["docs"]["command"], "docs-mcp");
    }
}
