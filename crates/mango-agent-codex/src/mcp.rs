//! Host MCP entries as app-server's per-thread configuration override.

use mango_external_agents::{Error, McpServer, McpTransport, Result, http_endpoint};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
                if http_endpoint::is_http_url(url)
                    && headers
                        .keys()
                        .all(|name| http_endpoint::is_http_field_name(name))
                    && headers
                        .values()
                        .all(|value| http_endpoint::is_http_field_value(value)) =>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_reject_separators_and_whitespace() {
        let with_header = |name: &str| {
            [McpServer {
                name: String::from("remote"),
                transport: McpTransport::Http {
                    url: String::from("https://docs.example/mcp"),
                    headers: BTreeMap::from([(String::from(name), String::from("value"))]),
                },
            }]
        };
        assert!(matches!(
            override_for(&with_header("X-Docs_Key")),
            Ok(Some(_))
        ));
        for name in ["X Docs", "X:Docs", ""] {
            assert!(
                matches!(
                    override_for(&with_header(name)),
                    Err(Error::HostConfiguration { .. })
                ),
                "expected header name {name:?} to be refused before launch | received an override"
            );
        }
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

    #[test]
    fn http_mcp_headers_reject_values_the_http_library_cannot_encode() {
        let server = [McpServer {
            name: String::from("remote"),
            transport: McpTransport::Http {
                url: String::from("https://docs.example/mcp"),
                headers: BTreeMap::from([(String::from("X-Docs"), String::from("token\u{7f}"))]),
            },
        }];

        assert!(
            matches!(override_for(&server), Err(Error::HostConfiguration { .. })),
            "expected a header value the HTTP library rejects to fail before launch"
        );
    }
}
