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
//! The host supplies the scratch root that the launched child can read. The file is written into a
//! directory of its own below that root, named unguessably and created with owner-only permissions
//! on Unix. On Windows it inherits the host root's ACL. It is removed when the session that wrote
//! it is closed or dropped.
//!
//! <https://code.claude.com/docs/en/mcp.md>

use std::io::Write;
use std::path::{Path, PathBuf};

use mango_external_agents::{Error, McpServer, McpTransport, Result};
use serde_json::{Map, Value, json};

trait FileWriter {
    fn write_new(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;
}

struct PrivateFileWriter;

impl FileWriter for PrivateFileWriter {
    fn write_new(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        write_private_file(path, contents)
    }
}

/// The name of the file inside each session's own directory.
const FILE_NAME: &str = "mcp-servers.json";

/// One session's `--mcp-config` file, removed when it is dropped.
pub struct ConfigFile {
    directory: PathBuf,
    /// The file's path, as the string `--mcp-config` takes. UTF-8 was already proven when this was
    /// written, so [`path`](Self::path) can hand it back as a [`Path`] without a second check.
    argument: String,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigFile")
            .field("artifact_owned", &true)
            .finish_non_exhaustive()
    }
}

impl ConfigFile {
    /// Writes the servers a host configured, or nothing when it configured none.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when a server is missing the name or the endpoint the vendor
    /// needs, and [`Error::HostConfiguration`] when the supplied scratch directory cannot hold the
    /// file. Both refuse the session:
    /// starting one that silently dropped a server the host asked for would run turns without the
    /// tools somebody configured.
    pub async fn write(servers: &[McpServer], scratch: &Path) -> Result<Option<Self>> {
        Self::write_on_blocking_pool(servers, scratch, PrivateFileWriter).await
    }

    /// The write, on the blocking pool, with the writer injected so a test can watch the thread.
    async fn write_on_blocking_pool(
        servers: &[McpServer],
        scratch: &Path,
        writer: impl FileWriter + Send + 'static,
    ) -> Result<Option<Self>> {
        let servers = servers.to_vec();
        let scratch = scratch.to_path_buf();
        // Every step of the write is a synchronous filesystem call, and the host owns the scratch
        // root: it may sit on a FUSE, container or network mount whose `metadata` alone takes
        // seconds. Opening one session must not park the async worker that every other session on
        // that runtime is rendering its turn on.
        tokio::task::spawn_blocking(move || Self::write_with(&servers, &scratch, &writer))
            .await
            .map_err(|_| Error::HostConfiguration {
                expected: "a blocking pool that can run the MCP configuration write",
                received: String::from("a blocking task that did not finish"),
            })?
    }

    fn write_with(
        servers: &[McpServer],
        scratch: &Path,
        writer: &impl FileWriter,
    ) -> Result<Option<Self>> {
        if servers.is_empty() {
            return Ok(None);
        }
        // The position, never the name. A server name is host-provided text — a tenant, a customer,
        // a URL with a credential in it — and `HostConfiguration`'s summary is written verbatim by
        // `Display`, so the index is what identifies the entry without carrying it.
        if let Some(index) = servers.iter().position(|server| !server.is_usable()) {
            return Err(Error::HostConfiguration {
                expected: "every MCP server to name itself and its command or url",
                received: format!("an MCP server at index {index} missing one of them"),
            });
        }

        // Every refusal that does not touch the disk runs before the directory below is created,
        // so a host misconfiguration never leaves an unguessable, owner-only directory behind for
        // nobody to clean up.
        let document = document_for(servers)?;
        validate_scratch(scratch)?;

        let directory = scratch.join(uuid::Uuid::new_v4().to_string());
        let path = directory.join(FILE_NAME);
        // `--mcp-config` takes one argument, and an argument is a string. A path that is not UTF-8
        // would have to be rendered lossily, and a lossy path names a different file.
        let Some(argument) = path.to_str().map(str::to_owned) else {
            return Err(Error::HostConfiguration {
                expected: "a scratch directory whose path is valid UTF-8",
                received: String::from("a non-UTF-8 path"),
            });
        };
        // The same predicate `TurnArgv::build` applies to `--mcp-config`, against the finished
        // path rather than the root, because the leaf directory and the file name are part of what
        // the cap measures. Checked before the directory exists: a scratch root the CLI cannot be
        // handed — one holding a newline, say, which every Unix filesystem accepts — would
        // otherwise open a session, write the credential-bearing artifact, and fail every turn it
        // is asked for.
        if !mango_external_agents::normalize::is_argv_value_with_max(
            &argument,
            mango_external_agents::normalize::MAX_PATH_LENGTH,
        ) {
            return Err(Error::HostConfiguration {
                expected: "a scratch path that can occupy a --mcp-config value",
                received: crate::argv::value_summary(&argument),
            });
        }

        create_private_directory(&directory)?;
        // Owned from the moment the directory exists, so every failure below returns through this
        // value's `Drop` instead of leaving an unguessable, owner-only directory — with the
        // credential-carrying file inside it — behind for nobody to clean up.
        let file = Self {
            directory,
            argument,
        };
        file.populate(&document, writer)?;
        Ok(Some(file))
    }

    /// Writes the document into the directory this value already owns.
    fn populate(&self, document: &Value, writer: &impl FileWriter) -> Result<()> {
        let path = self.path();
        let contents = document.to_string();
        writer
            .write_new(path, contents.as_bytes())
            .map_err(|error| scratch_failure("write the MCP configuration", &error))?;
        Ok(())
    }

    /// The path `--mcp-config` is given.
    pub fn path(&self) -> &Path {
        Path::new(&self.argument)
    }

    /// That path as the argument a turn's argv carries.
    pub fn argument(&self) -> &str {
        &self.argument
    }
}

impl Drop for ConfigFile {
    /// Removes the whole directory, so a session that was never closed still leaves nothing behind.
    ///
    /// Synchronous, deliberately: a value dropped without a close has no runtime to hand the call
    /// to, and by the time the last session drops the runtime may already be shutting down. A
    /// close that does have one goes through `release_off_worker` instead, and a call that was
    /// cancelled mid-open through `Prepared`.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// A configuration artifact owned by a call that may be cancelled before it finishes.
///
/// `open_session` writes the file, then awaits three child processes before the session can take
/// it. A caller that drops that future in between drops the artifact on the async worker, and
/// `ConfigFile`'s own `Drop` would run `remove_dir_all` there — the same stall the write, the
/// close and the raced turn were all moved off. Cancellation is not a refusal: there is no error
/// path to release on and nothing left to await, so the removal is handed off and left to finish.
pub(crate) struct Prepared(Option<ConfigFile>);

impl Prepared {
    /// Takes ownership of what an open just wrote, if it wrote anything.
    pub(crate) fn new(file: Option<ConfigFile>) -> Self {
        Self(file)
    }

    /// Hands the artifact to whoever owns it next, leaving nothing for this value to remove.
    pub(crate) fn take(&mut self) -> Option<ConfigFile> {
        self.0.take()
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let Some(file) = self.0.take() else {
            return;
        };
        release_on_drop(file);
    }
}

/// Hands a value's `Drop` to the blocking pool when there is a runtime to hand it to.
///
/// Fire and forget, because a `Drop` cannot await. A thread with no runtime — a host that built
/// the session outside one, or a runtime already shutting down — drops it where it stands, which
/// is what the value would have done anyway.
fn release_on_drop<T: Send + 'static>(value: T) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || drop(value));
        }
        Err(_) => drop(value),
    }
}

/// Releases a configuration artifact without running the host's filesystem on the async worker.
///
/// Dropping the last reference removes the directory with a synchronous `remove_dir_all` against
/// a root the host chose, which may be a FUSE, container or network mount — the same call
/// [`ConfigFile::write`] runs on the blocking pool for the same reason. A reference that is not
/// the last one costs a blocking task that does nothing, which is the cheaper half of the trade.
///
/// Awaited rather than detached: a close that returned before the artifact was gone would be a
/// close that did not clean up.
///
/// Generic over what is being released because the owner differs by path: an open that fails
/// holds the [`ConfigFile`] itself, a session holds an `Arc` of it, and a turn holds a lease that
/// is the last `Arc` whenever a close raced it. Every one of them can be the reference whose drop
/// unlinks the directory.
pub(crate) async fn release_off_worker<T: Send + 'static>(value: Option<T>) {
    let Some(value) = value else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || drop(value)).await;
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
    for (index, server) in servers.iter().enumerate() {
        let entry = entry_for(&server.transport).ok_or_else(|| Error::HostConfiguration {
            expected: "an MCP transport this harness maps onto --mcp-config",
            // Neither the transport nor the name: `Stdio` carries `env` and `Http` carries
            // `headers`, and both are where a host puts the credential its server authenticates
            // with, while the name is host-provided text of its own.
            received: format!("an unmapped transport on the server at index {index}"),
        })?;
        // The vendor's `mcpServers` is a map, so a repeated name can only keep one entry — and
        // `insert` would keep the last quietly. That is the same drop this module refuses an
        // unusable transport for: a turn that runs without the tools somebody configured, reported
        // as though it had been set up. Which of the two the host meant is not this harness's guess
        // to make.
        if configured.insert(server.name.clone(), entry).is_some() {
            let count = servers
                .iter()
                .filter(|other| other.name == server.name)
                .count();
            return Err(Error::HostConfiguration {
                expected: "one MCP server per name, because the vendor lists them in a map",
                received: format!("{count} servers sharing the name at index {index}"),
            });
        }
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

/// Creates a new owner-only session directory directly under the host-owned scratch root.
///
/// The root already exists and belongs to the host. This code never creates or changes it, because
/// doing either would follow a host-owned path the library was never authorised to mutate. The
/// UUID makes independent sessions distinct and `create` refuses a collision instead of reusing a
/// path somebody else supplied.
#[cfg(unix)]
fn create_private_directory(directory: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(directory)
        .map_err(|error| scratch_failure("create an owner-only session directory", &error))
}

/// Windows inherits the directory ACL from the host-owned scratch root. The host supplies that
/// root because only it knows the account, container mount or sandbox ACL the child shares.
#[cfg(not(unix))]
fn create_private_directory(directory: &Path) -> Result<()> {
    std::fs::create_dir(directory)
        .map_err(|error| scratch_failure("create a session directory", &error))
}

/// Creates one private MCP file without replacing an existing path.
#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)
}

/// Windows inherits the file ACL from its private session directory and fails if the filename is
/// already present. That keeps the host's ACL policy intact while preserving no-overwrite semantics.
#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(contents)
}

fn validate_scratch(scratch: &Path) -> Result<()> {
    if !scratch.is_absolute() {
        return Err(Error::HostConfiguration {
            expected: "an absolute UTF-8 scratch directory visible to the Claude child",
            received: String::from("a relative path"),
        });
    }
    if scratch.to_str().is_none() {
        return Err(Error::HostConfiguration {
            expected: "an absolute UTF-8 scratch directory visible to the Claude child",
            received: String::from("a non-UTF-8 path"),
        });
    }
    let metadata = std::fs::metadata(scratch)
        .map_err(|error| scratch_failure("read the scratch directory", &error))?;
    if !metadata.is_dir() {
        return Err(Error::HostConfiguration {
            expected: "an existing directory visible to the Claude child",
            received: String::from("a non-directory path"),
        });
    }

    #[cfg(unix)]
    validate_unix_scratch_root(&metadata)?;

    Ok(())
}

/// Refuses a root whose owner could replace a session leaf before Claude opens it.
///
/// A private root is safe. A shared root also is safe only with the sticky bit: it prevents an
/// unrelated directory user from renaming the leaf this process owns. Root-owned sticky roots
/// cover the platform temporary directory without trusting a non-root third party to retain the
/// configuration the host asked Claude to load.
#[cfg(unix)]
fn validate_unix_scratch_root(metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = metadata.mode();
    let owner = metadata.uid();
    let effective = nix::unistd::Uid::effective().as_raw();
    if safe_unix_scratch_root(mode, owner, effective) {
        return Ok(());
    }

    Err(Error::HostConfiguration {
        expected: "a scratch directory owned by this user or root, and private or sticky when group- or other-writable",
        received: String::from("an unsafe Unix scratch directory"),
    })
}

/// Whether Unix directory ownership and mode prevent a third party from replacing a session leaf.
#[cfg(unix)]
fn safe_unix_scratch_root(mode: u32, owner: u32, effective: u32) -> bool {
    let owner_is_trusted = owner == effective || owner == 0;
    let shared_for_rename = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    owner_is_trusted && (!shared_for_rename || sticky)
}

fn scratch_failure(what: &str, error: &std::io::Error) -> Error {
    Error::HostConfiguration {
        expected: "a writable host-owned scratch directory for MCP configuration",
        received: format!("could not {what}: {:?}", error.kind()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[cfg(unix)]
    use super::safe_unix_scratch_root;
    use super::{ConfigFile, FILE_NAME, FileWriter, document_for, write_private_file};
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
    fn refuses_two_servers_that_claim_the_same_name() {
        let mut servers = servers();
        servers.push(McpServer::stdio("docs", "other-docs-mcp"));

        let error = document_for(&servers).expect_err("expected the collision to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration {
                    expected: "one MCP server per name, because the vendor lists them in a map",
                    ..
                }
            ),
            "expected the collision refusal, received {error:?}"
        );
        // `servers()`'s first `docs` entry carries `DOCS_TOKEN=s3cret`. Naming the collision is
        // enough to diagnose it; the credential the first entry mapped to is not.
        assert!(
            !error.to_string().contains("s3cret"),
            "expected the collision's mapped value to stay out of the message, received {error}"
        );
    }

    /// A server name is host text, and `HostConfiguration` is the one summary `Display` writes out.
    ///
    /// The name is whatever the host called the server: a tenant, a customer, a URL somebody pasted
    /// with a credential still in it. Every refusal on this path identifies the entry by position.
    #[test]
    fn no_configuration_refusal_names_the_server_a_host_configured() {
        const CANARY: &str = "tenant-secret-canary";

        // Named, but with neither a command nor a url.
        let unusable = vec![McpServer {
            name: String::from(CANARY),
            transport: McpTransport::Stdio {
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
            },
        }];
        // Two entries the vendor's map cannot hold at once.
        let collision = vec![
            McpServer::stdio(CANARY, "docs-mcp"),
            McpServer::stdio(CANARY, "other-docs-mcp"),
        ];

        let refusals = [
            ConfigFile::write_with(&unusable, Path::new("/tmp/mea-mcp"), &FailingWriter)
                .expect_err("expected an unusable server to be refused"),
            document_for(&collision).expect_err("expected the collision to be refused"),
        ];

        for error in refusals {
            let rendered = error.to_string();
            assert!(
                !rendered.contains(CANARY),
                "expected the server name to stay out of the diagnostic, received {rendered}"
            );
            assert!(
                rendered.contains("index"),
                "expected the entry to be identified by position, received {rendered}"
            );
        }
    }

    /// A writer that fails if anything reaches the disk, so a refusal test cannot write one.
    struct FailingWriter;

    impl FileWriter for FailingWriter {
        fn write_new(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
            panic!(
                "expected no write, received one of {} bytes to {path:?}",
                contents.len()
            )
        }
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
    async fn refuses_relative_or_non_directory_scratch_without_writing_anything() {
        let relative = ConfigFile::write(&servers(), Path::new("relative-scratch"))
            .await
            .expect_err("expected relative scratch to be refused");
        assert!(
            matches!(
                relative,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {relative:?}"
        );

        let scratch = tempdir();
        let file = scratch.join("not-a-directory");
        std::fs::write(&file, "host file").expect("expected a host file");
        let non_directory = ConfigFile::write(&servers(), &file)
            .await
            .expect_err("expected a non-directory scratch path to be refused");
        assert!(
            matches!(
                non_directory,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {non_directory:?}"
        );
        let _ = std::fs::remove_dir_all(scratch);
    }

    /// A path every Unix filesystem accepts and `--mcp-config` cannot carry.
    ///
    /// `TurnArgv::build` refuses a control character in an argv value, so a session opened over
    /// this root would write the credential-bearing artifact, return successfully, and then fail
    /// every turn it was asked for.
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_scratch_path_the_turn_argv_could_not_carry() {
        let root = tempdir();
        let scratch = root.join("holds\na newline");
        std::fs::create_dir(&scratch).expect("expected a host-owned scratch directory");

        let error = ConfigFile::write(&servers(), &scratch)
            .await
            .expect_err("expected an unusable scratch path to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration {
                    expected: "a scratch path that can occupy a --mcp-config value",
                    ..
                }
            ),
            "expected the argv-value refusal, received {error:?}"
        );
        assert_eq!(
            std::fs::read_dir(&scratch)
                .expect("expected the host scratch root")
                .count(),
            0,
            "expected no artifact beneath a scratch root the CLI cannot be handed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Opening a session must not run the host's filesystem on the async worker.
    ///
    /// Every step of the write is a synchronous call, and the host's scratch root may be a FUSE,
    /// container or network mount whose `metadata` alone takes seconds. Run inline, one slow
    /// mount stalls every session on that runtime.
    ///
    /// Asserted on the thread the write ran on rather than on whether another task got a turn:
    /// a `spawn_blocking` whose closure has already finished can be awaited without yielding, so
    /// "some other task progressed" passes or fails on how warm the blocking pool is. On a
    /// current-thread runtime that pool is provably a different thread.
    #[tokio::test(flavor = "current_thread")]
    async fn writing_the_configuration_runs_off_the_async_worker() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::thread::ThreadId;

        /// Writes for real, and reports which thread it was called on.
        struct ThreadRecordingWriter(Arc<Mutex<Option<ThreadId>>>);

        impl FileWriter for ThreadRecordingWriter {
            fn write_new(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
                *self.0.lock().expect("an uncontended recorder") =
                    Some(std::thread::current().id());
                write_private_file(path, contents)
            }
        }

        let scratch = tempdir();
        let wrote_on = Arc::new(Mutex::new(None));
        let file = ConfigFile::write_on_blocking_pool(
            &servers(),
            &scratch,
            ThreadRecordingWriter(Arc::clone(&wrote_on)),
        )
        .await
        .expect("expected a file")
        .expect("expected servers to produce one");

        let wrote_on = *wrote_on.lock().expect("an uncontended recorder");
        assert!(
            wrote_on.is_some(),
            "expected the writer to have been called"
        );
        assert_ne!(
            wrote_on,
            Some(std::thread::current().id()),
            "expected the write to run on the blocking pool, received the async worker"
        );
        drop(file);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Closing a session must not run the host's filesystem on the async worker either.
    ///
    /// The mirror of the write: `remove_dir_all` is the same synchronous call against the same
    /// host-chosen root, and `ClaudeSession::close` drops the last reference.
    ///
    /// Asserted on the thread the `Drop` ran on rather than on whether another task got a turn.
    /// A `spawn_blocking` whose closure has already finished can be awaited without yielding, so
    /// "some other task progressed" is a race; "not this thread" is the property itself, and on a
    /// current-thread runtime the blocking pool is provably a different thread.
    #[tokio::test(flavor = "current_thread")]
    async fn releasing_the_configuration_runs_its_drop_off_the_async_worker() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::thread::ThreadId;

        /// Reports which thread dropped it.
        struct ThreadProbe(Arc<Mutex<Option<ThreadId>>>);

        impl Drop for ThreadProbe {
            fn drop(&mut self) {
                *self.0.lock().expect("an uncontended probe") = Some(std::thread::current().id());
            }
        }

        let dropped_on = Arc::new(Mutex::new(None));
        super::release_off_worker(Some(ThreadProbe(Arc::clone(&dropped_on)))).await;

        let dropped_on = *dropped_on.lock().expect("an uncontended probe");
        assert!(
            dropped_on.is_some(),
            "expected the value to have been dropped, received a live one"
        );
        assert_ne!(
            dropped_on,
            Some(std::thread::current().id()),
            "expected the drop to run on the blocking pool, received the async worker"
        );
    }

    /// A cancelled open has no error path to release on, so the `Drop` has to do the handing off.
    ///
    /// `open_session` writes the artifact and then awaits three child processes. A caller that
    /// drops that future in between reaches no return at all, and `ConfigFile`'s own `Drop` would
    /// run `remove_dir_all` on the thread polling it. The oneshot makes the assertion
    /// deterministic: it resolves inside the drop, so there is nothing to wait out.
    #[tokio::test(flavor = "current_thread")]
    async fn a_prepared_artifact_dropped_without_a_return_still_leaves_the_worker() {
        use std::thread::ThreadId;

        /// Reports which thread dropped it, before the receiver can be polled.
        struct ThreadProbe(Option<tokio::sync::oneshot::Sender<ThreadId>>);

        impl Drop for ThreadProbe {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(std::thread::current().id());
                }
            }
        }

        let (sender, receiver) = tokio::sync::oneshot::channel();
        super::release_on_drop(ThreadProbe(Some(sender)));

        let dropped_on = receiver.await.expect("expected the probe to be dropped");
        assert_ne!(
            dropped_on,
            std::thread::current().id(),
            "expected the drop to run on the blocking pool, received the async worker"
        );
    }

    /// Outside a runtime there is nothing to hand it to, so it is dropped where it stands.
    #[test]
    fn a_prepared_artifact_dropped_without_a_runtime_is_removed_in_place() {
        let scratch = tempdir();
        let directory = {
            let file = ConfigFile::write_with(&servers(), &scratch, &super::PrivateFileWriter)
                .expect("expected a file")
                .expect("expected servers to produce one");
            let directory = file
                .path()
                .parent()
                .expect("a file sits in a directory")
                .to_path_buf();
            drop(super::Prepared::new(Some(file)));
            directory
        };

        assert!(
            !directory.exists(),
            "expected the artifact to be gone, received {directory:?}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// And the artifact is gone by the time the release returns, not merely scheduled.
    #[tokio::test]
    async fn releasing_the_configuration_removes_the_artifact_before_it_returns() {
        use std::sync::Arc;

        let scratch = tempdir();
        let file = Arc::new(
            ConfigFile::write(&servers(), &scratch)
                .await
                .expect("expected a file")
                .expect("expected servers to produce one"),
        );
        let directory = file
            .path()
            .parent()
            .expect("a file sits in a directory")
            .to_path_buf();

        super::release_off_worker(Some(file)).await;

        assert!(
            !directory.exists(),
            "expected the artifact to be gone once the release returned, received {directory:?}"
        );
        let _ = std::fs::remove_dir_all(&scratch);
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
    async fn refuses_colliding_names_without_leaving_a_directory_behind() {
        let scratch = tempdir();
        let mut colliding = servers();
        colliding.push(McpServer::stdio("docs", "other-docs-mcp"));

        let error = ConfigFile::write(&colliding, &scratch)
            .await
            .map(|file| file.is_some())
            .expect_err("expected the collision to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
        );

        let left_behind = std::fs::read_dir(&scratch)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            left_behind, 0,
            "expected a refusal that never wrote a file to leave no directory behind, received {left_behind} entries"
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

    #[tokio::test]
    async fn formatting_a_config_file_does_not_reveal_its_scratch_path() {
        let scratch = tempdir().join("customer-secret");
        std::fs::create_dir(&scratch).expect("expected a host-owned scratch directory");
        let file = ConfigFile::write(&servers(), &scratch)
            .await
            .expect("expected a file")
            .expect("expected servers to produce one");

        let rendered = format!("{file:?}");
        assert!(
            !rendered.contains("present"),
            "expected Debug to report stable ownership metadata without probing the filesystem, received {rendered}"
        );
        assert!(
            !rendered.contains("customer-secret"),
            "expected scratch path to stay out of debug output, received {rendered}"
        );
        drop(file);
        let _ =
            std::fs::remove_dir_all(scratch.parent().expect("expected the dedicated test root"));
    }

    #[tokio::test]
    async fn simultaneous_sessions_use_distinct_artifacts_and_clean_up_only_their_own() {
        let scratch = tempdir();
        let first = ConfigFile::write(&servers(), &scratch)
            .await
            .expect("expected the first file")
            .expect("expected servers to produce one");
        let second = ConfigFile::write(&servers(), &scratch)
            .await
            .expect("expected the second file")
            .expect("expected servers to produce one");
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();

        assert_ne!(
            first_path, second_path,
            "expected isolated session artifacts"
        );
        drop(first);
        assert!(
            second_path.exists(),
            "expected one session cleanup to preserve {}",
            second_path.display()
        );
        drop(second);
        assert!(
            !first_path.exists() && !second_path.exists(),
            "expected both session artifacts to be removed"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_private_file_write_never_replaces_an_existing_file() {
        let scratch = tempdir();
        let path = scratch.join(FILE_NAME);
        std::fs::write(&path, "host-owned contents").expect("expected an existing host file");

        let error = write_private_file(&path, b"replacement")
            .expect_err("expected the existing path to be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&path).expect("expected the host file to remain"),
            "host-owned contents"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// The failure that only becomes reachable once the directory exists.
    ///
    /// Everything before `create_private_directory` refuses without touching the disk, so the only
    /// way to leave one of these behind is a failure *after* it — and what is left is an
    /// unguessable, owner-only directory that nothing will ever clean up, holding the file a host
    /// put its own server's credential in.
    ///
    /// A named writer fails after the production path creates and takes ownership of the session
    /// directory. This avoids relying on platform-specific path-length limits.
    #[test]
    fn a_failure_after_the_directory_exists_still_leaves_nothing_behind() {
        struct FailingFileWriter;

        impl FileWriter for FailingFileWriter {
            fn write_new(&self, _path: &Path, _contents: &[u8]) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected write refusal",
                ))
            }
        }

        let scratch = tempdir();
        let error = ConfigFile::write_with(&servers(), &scratch, &FailingFileWriter)
            .expect_err("expected the write to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
        );
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration {
                    expected: "a writable host-owned scratch directory for MCP configuration",
                    ..
                }
            ),
            "expected the injected post-directory failure, received {error:?}"
        );

        let left_behind = std::fs::read_dir(&scratch)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            left_behind, 0,
            "expected a failed write to take its own directory with it, received {left_behind} entries"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// A shared, non-sticky Unix root lets another account rename a session leaf before Claude
    /// opens the configuration file. Refuse it without creating anything below the host root.
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_a_non_sticky_shared_scratch_root_without_creating_a_leaf() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempdir();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o777))
            .expect("expected a non-sticky shared root");

        let error = ConfigFile::write(&servers(), &scratch)
            .await
            .expect_err("expected a non-sticky shared root to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration {
                    expected: "a scratch directory owned by this user or root, and private or sticky when group- or other-writable",
                    ..
                }
            ),
            "expected an explicit scratch-root refusal, received {error:?}"
        );

        assert_eq!(
            std::fs::read_dir(&scratch)
                .expect("expected the host scratch root")
                .count(),
            0,
            "expected the refusal to leave no session leaf beneath the host root"
        );
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700))
            .expect("expected cleanup permissions");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[test]
    fn unix_scratch_root_safety_requires_a_trusted_owner_and_sticky_shared_mode() {
        assert!(safe_unix_scratch_root(0o700, 1000, 1000));
        assert!(safe_unix_scratch_root(0o1777, 0, 1000));
        assert!(!safe_unix_scratch_root(0o777, 1000, 1000));
        assert!(!safe_unix_scratch_root(0o1777, 1001, 1000));
    }

    /// A directory of this test's own, so a run never touches another's.
    fn tempdir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("mea-mcp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("expected a scratch directory");
        path
    }
}
