//! Host MCP entries as app-server's per-thread configuration override.

use std::collections::BTreeMap;

use mango_external_agents::{Error, McpServer, McpTransport, Result};
use serde_json::{Value, json};

/// One request-only config map; nothing here writes the user's `config.toml`.
pub(crate) fn override_for(servers: &[McpServer]) -> Result<Option<BTreeMap<String, Value>>> {
    if servers.is_empty() {
        return Ok(None);
    }
    let mut entries = serde_json::Map::new();
    for (index, server) in servers.iter().enumerate() {
        if !server.is_usable() || server.name.chars().any(char::is_control) {
            return Err(Error::HostConfiguration {
                expected: "an MCP server with a nonempty name and transport target",
                received: format!("invalid MCP server at index {index}"),
            });
        }
        if entries.contains_key(&server.name) {
            return Err(Error::HostConfiguration {
                expected: "unique MCP server names",
                received: format!("duplicate MCP server at index {index}"),
            });
        }
        let config = match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                json!({"command": command, "args": args, "env": env})
            }
            McpTransport::Http { url, headers }
                if valid_http_mcp_url(url)
                    && headers.keys().all(|name| valid_header_name(name))
                    && headers
                        .values()
                        .all(|value| !value.contains('\r') && !value.contains('\n')) =>
            {
                json!({"url": url, "http_headers": headers})
            }
            McpTransport::Http { .. } => {
                return Err(Error::HostConfiguration {
                    expected: "an HTTP MCP URL and valid header names and values",
                    received: format!("invalid HTTP MCP server at index {index}"),
                });
            }
            _ => {
                return Err(Error::HostConfiguration {
                    expected: "an MCP stdio or streamable HTTP transport",
                    received: format!("unsupported MCP server transport at index {index}"),
                });
            }
        };
        entries.insert(server.name.clone(), config);
    }
    Ok(Some(BTreeMap::from([(
        String::from("mcp_servers"),
        Value::Object(entries),
    )])))
}

/// Whether an MCP endpoint is an absolute HTTP URL that the app-server can safely pass through.
fn valid_http_mcp_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host = authority.host();
    let Some(after_host) = authority.as_str().strip_prefix(host) else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && !host.is_empty()
        && !authority.as_str().contains('@')
        && (after_host.is_empty()
            || (after_host.starts_with(':') && authority.port_u16().is_some()))
        && !url.contains('#')
        && url
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

/// HTTP field names use the token grammar; no whitespace or separator can change a header.
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_reject_separators_and_whitespace() {
        assert!(valid_header_name("X-Docs_Key"));
        assert!(!valid_header_name("X Docs"));
        assert!(!valid_header_name("X:Docs"));
        assert!(!valid_header_name(""));
    }

    #[test]
    fn invalid_and_duplicate_entries_are_refused_without_naming_secret_values() {
        let duplicate = [
            McpServer::stdio("docs", "one"),
            McpServer::stdio("docs", "two"),
        ];
        assert!(matches!(
            override_for(&duplicate),
            Err(Error::HostConfiguration { .. })
        ));
        let bad_http = [McpServer {
            name: String::from("remote"),
            transport: McpTransport::Http {
                url: String::from("ftp://example.com/mcp?secret=canary"),
                headers: BTreeMap::new(),
            },
        }];
        let error = override_for(&bad_http).expect_err("expected an unsupported URL to fail");
        assert!(!format!("{error:?}").contains("canary"));
    }

    #[test]
    fn http_mcp_urls_require_an_absolute_authority_without_whitespace() {
        for url in [
            "http://",
            "https://docs.example/mcp endpoint",
            "https://docs.example/mcp\tendpoint",
        ] {
            let server = [McpServer {
                name: String::from("remote"),
                transport: McpTransport::Http {
                    url: String::from(url),
                    headers: BTreeMap::new(),
                },
            }];
            assert!(
                matches!(override_for(&server), Err(Error::HostConfiguration { .. })),
                "expected {url:?} to be rejected before launch"
            );
        }
    }
}
