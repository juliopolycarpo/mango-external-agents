//! MCP servers the host configured, handed to Claude Code as `--mcp-config`.
//!
//! The vendor documents the flag as "Load MCP servers from JSON files or strings", and the file's
//! shape as a `mcpServers` map of name to config: `command`, `args` and `env` for a server the CLI
//! spawns, and `type`, `url` and `headers` for one it dials. An entry with a `url` and no `type` is
//! a documented configuration error, so the HTTP arm always writes one.
//!
//! A **file**, never the inline-string form the same flag accepts: an inline config would put the
//! whole server list — headers and environment included — on a command line that is world-readable
//! in `ps` on every platform this runs on.
//!
//! The file is written into a directory of its own, named unguessably and created with owner-only
//! permissions where the platform has them, because an `env` entry is exactly where a host puts its
//! own server's API key. It is removed when the session that wrote it is closed or dropped.
//!
//! <https://code.claude.com/docs/en/mcp.md>

use std::path::{Path, PathBuf};

use mango_external_agents::{Error, McpServer, McpTransport, Result};
use serde_json::{Map, Value, json};

/// The directory every session's configuration is written under.
const PARENT_DIRECTORY: &str = "mango-external-agents";

/// The name of the file inside each session's own directory.
const FILE_NAME: &str = "mcp-servers.json";

/// One session's `--mcp-config` file, removed when it is dropped.
#[derive(Debug)]
pub struct ConfigFile {
    directory: PathBuf,
    path: PathBuf,
    argument: String,
}

impl ConfigFile {
    /// Writes the servers a host configured, or nothing when it configured none.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when a server is missing the name or the endpoint the vendor
    /// needs, and [`Error::Launch`] when the file could not be written. Both refuse the session:
    /// starting one that silently dropped a server the host asked for would run turns without the
    /// tools somebody configured.
    pub async fn write(servers: &[McpServer], scratch: &Path) -> Result<Option<Self>> {
        if servers.is_empty() {
            return Ok(None);
        }
        if let Some(unusable) = servers.iter().find(|server| !server.is_usable()) {
            return Err(Error::HostConfiguration {
                expected: "every MCP server to name itself and its command or url",
                received: format!("{:?}", unusable.name),
            });
        }

        let directory = scratch
            .join(PARENT_DIRECTORY)
            .join(uuid::Uuid::new_v4().to_string());
        create_private_directory(&directory).await?;

        let path = directory.join(FILE_NAME);
        // `--mcp-config` takes one argument, and an argument is a string. A path that is not UTF-8
        // would have to be rendered lossily, and a lossy path names a different file.
        let Some(argument) = path.to_str().map(str::to_owned) else {
            return Err(Error::HostConfiguration {
                expected: "a scratch directory whose path is valid UTF-8",
                received: path.to_string_lossy().into_owned(),
            });
        };
        let document = document_for(servers)?;
        tokio::fs::write(&path, document.to_string())
            .await
            .map_err(|error| launch_failure("write an MCP configuration", &path, &error))?;
        restrict_to_owner(&path).await?;

        Ok(Some(Self {
            directory,
            path,
            argument,
        }))
    }

    /// The path `--mcp-config` is given.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// That path as the argument a turn's argv carries.
    pub fn argument(&self) -> &str {
        &self.argument
    }
}

impl Drop for ConfigFile {
    /// Removes the whole directory, so a session that was never closed still leaves nothing behind.
    ///
    /// Synchronous, deliberately: this is one `rmdir` of one small local directory, and an async
    /// cleanup would need a runtime that may already be shutting down when the session drops.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// The `mcpServers` document, as the vendor's own configuration files spell it.
///
/// # Errors
///
/// [`Error::HostConfiguration`] for a transport kind this harness does not map. [`McpTransport`]
/// is `#[non_exhaustive]`, so the core can grow one — and a server written into the file under a
/// guessed shape would be a server the vendor cannot reach, reported as though it had been
/// configured.
fn document_for(servers: &[McpServer]) -> Result<Value> {
    let mut configured = Map::new();
    for server in servers {
        let entry = entry_for(&server.transport).ok_or_else(|| Error::HostConfiguration {
            expected: "an MCP transport this harness maps onto --mcp-config",
            received: format!("{:?} on server {:?}", server.transport, server.name),
        })?;
        configured.insert(server.name.clone(), entry);
    }
    Ok(json!({ "mcpServers": Value::Object(configured) }))
}

fn entry_for(transport: &McpTransport) -> Option<Value> {
    Some(match transport {
        // No `type` key: the vendor reads an entry without one as a stdio server, which is what
        // this is. Empty `args` and `env` are omitted rather than written as empty containers.
        McpTransport::Stdio { command, args, env } => {
            let mut entry = Map::new();
            entry.insert(String::from("command"), json!(command));
            if !args.is_empty() {
                entry.insert(String::from("args"), json!(args));
            }
            if !env.is_empty() {
                entry.insert(String::from("env"), json!(env));
            }
            Value::Object(entry)
        }
        // `type` is mandatory here: the vendor documents an entry with a `url` and no `type` as a
        // configuration error it reports and skips.
        McpTransport::Http { url, headers } => {
            let mut entry = Map::new();
            entry.insert(String::from("type"), json!("http"));
            entry.insert(String::from("url"), json!(url));
            if !headers.is_empty() {
                entry.insert(String::from("headers"), json!(headers));
            }
            Value::Object(entry)
        }
        _ => return None,
    })
}

/// Creates the session's own directory, owner-only where the platform says what that means.
///
/// `create_dir_all` for the parent, which several sessions share, and a plain `create_dir` for the
/// leaf: a leaf that already exists is a name somebody else chose, and the name is a fresh UUID.
async fn create_private_directory(directory: &Path) -> Result<()> {
    if let Some(parent) = directory.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| launch_failure("create a scratch directory", parent, &error))?;
    }
    tokio::fs::create_dir(directory)
        .await
        .map_err(|error| launch_failure("create a scratch directory", directory, &error))?;
    restrict_to_owner(directory).await
}

/// Takes group and other off a path, on the platforms that have them.
///
/// A no-op on Windows, where the temporary directory is already per-user and the mode bits mean
/// nothing. Stated rather than silently skipped, because "the permissions were set" is a claim the
/// module's own docs make.
#[cfg(unix)]
async fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| launch_failure("read the permissions of", path, &error))?;
    let owner_only = if metadata.is_dir() { 0o700 } else { 0o600 };
    let mut permissions = metadata.permissions();
    permissions.set_mode(owner_only);
    tokio::fs::set_permissions(path, permissions)
        .await
        .map_err(|error| launch_failure("restrict the permissions of", path, &error))
}

#[cfg(not(unix))]
async fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

fn launch_failure(what: &str, path: &Path, error: &std::io::Error) -> Error {
    Error::Launch {
        program: String::from(crate::probe::PROGRAM),
        message: format!("expected to {what} {}, received: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigFile, document_for};
    use mango_external_agents::{McpServer, McpTransport};
    use serde_json::json;

    fn servers() -> Vec<McpServer> {
        vec![
            McpServer {
                name: String::from("docs"),
                transport: McpTransport::Stdio {
                    command: String::from("npx"),
                    args: vec![String::from("-y"), String::from("docs-mcp@1.2.3")],
                    env: [(String::from("DOCS_TOKEN"), String::from("s3cret"))]
                        .into_iter()
                        .collect(),
                },
            },
            McpServer {
                name: String::from("notion"),
                transport: McpTransport::Http {
                    url: String::from("https://mcp.notion.com/mcp"),
                    headers: [(String::from("Authorization"), String::from("Bearer t"))]
                        .into_iter()
                        .collect(),
                },
            },
        ]
    }

    #[test]
    fn writes_the_shape_the_vendors_own_configuration_files_use() {
        assert_eq!(
            document_for(&servers()).expect("expected a document"),
            json!({
                "mcpServers": {
                    "docs": {
                        "command": "npx",
                        "args": ["-y", "docs-mcp@1.2.3"],
                        "env": { "DOCS_TOKEN": "s3cret" }
                    },
                    "notion": {
                        "type": "http",
                        "url": "https://mcp.notion.com/mcp",
                        "headers": { "Authorization": "Bearer t" }
                    }
                }
            })
        );
    }

    #[test]
    fn a_stdio_entry_carries_no_type_and_no_empty_containers() {
        let document =
            document_for(&[McpServer::stdio("bare", "bare-mcp")]).expect("expected a document");
        assert_eq!(
            document["mcpServers"]["bare"],
            json!({ "command": "bare-mcp" })
        );
    }

    #[tokio::test]
    async fn no_servers_means_no_file_and_no_flag() {
        let scratch = tempdir();
        assert!(
            ConfigFile::write(&[], &scratch)
                .await
                .expect("expected no error")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn refuses_a_server_the_vendor_could_not_use_rather_than_dropping_it() {
        let scratch = tempdir();
        let incomplete = vec![McpServer::stdio("", "docs-mcp")];
        let error = ConfigFile::write(&incomplete, &scratch)
            .await
            .map(|file| file.is_some())
            .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn writes_a_file_only_its_owner_can_read_and_removes_it_when_dropped() {
        let scratch = tempdir();
        let file = ConfigFile::write(&servers(), &scratch)
            .await
            .expect("expected a file")
            .expect("expected servers to produce one");
        let path = file.path().to_path_buf();

        let written = std::fs::read_to_string(&path).expect("expected the file to exist");
        assert!(written.contains("mcpServers"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("expected metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "expected the credential-carrying file to be owner-only"
            );
        }

        drop(file);
        assert!(
            !path.exists(),
            "expected a dropped session to leave nothing behind at {}",
            path.display()
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A directory of this test's own, so a run never touches another's.
    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("mea-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("expected a scratch directory");
        path
    }
}
