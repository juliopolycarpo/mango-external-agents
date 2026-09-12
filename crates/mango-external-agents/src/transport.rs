//! The other axis: how bytes reach the harness.
//!
//! A [`TransportKind`] answers "how does this process or endpoint speak", and a [`TransportSpec`]
//! carries what that kind needs to be opened. The two are separate from the harness kind on
//! purpose: the same Codex dialect rides a child's pipes or a dialled socket, and the same ACP
//! agent rides child pipes or HTTP. A harness declares which kinds it accepts and the library
//! refuses an undeclared pair before anything is spawned.

use std::fmt;
use std::path::PathBuf;

/// How bytes reach a harness.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TransportKind {
    /// A child the host's launcher spawned; the library owns line framing.
    Stdio,
    /// A dialled WebSocket URL; the library owns message framing.
    WebSocket,
    /// The official Agent Client Protocol connection, which owns its own framing.
    Acp,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stdio => "stdio",
            Self::WebSocket => "websocket",
            Self::Acp => "acp",
        })
    }
}

/// What one transport kind needs in order to be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportSpec {
    /// Spawn this argv through the host's launcher and frame its stdout by lines.
    Stdio(StdioSpec),
    /// Dial this URL.
    WebSocket(WsSpec),
    /// Hand the official ACP client one of its two carriers.
    Acp(AcpSpec),
}

impl TransportSpec {
    /// Which kind this spec opens.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{StdioSpec, TransportKind, TransportSpec};
    ///
    /// let spec = TransportSpec::Stdio(StdioSpec::new(["claude", "--version"]));
    /// assert_eq!(spec.kind(), TransportKind::Stdio);
    /// ```
    pub fn kind(&self) -> TransportKind {
        match self {
            Self::Stdio(_) => TransportKind::Stdio,
            Self::WebSocket(_) => TransportKind::WebSocket,
            Self::Acp(_) => TransportKind::Acp,
        }
    }
}

/// A child process to spawn and read by lines.
///
/// The working directory and the environment are absent on purpose. Both are host-owned — the cwd
/// is the directory the host already authorised, and the environment is the positive allowlist
/// that keeps a host's own secret out of a vendor child — and both are set by the library from
/// [`HostContext`](crate::HostContext) at spawn time. Omitting them here means a harness cannot
/// write a value that silently does nothing, and cannot appear to inject one that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdioSpec {
    /// The program and its arguments. The first element is the executable.
    pub argv: Vec<String>,
}

impl StdioSpec {
    /// Names the program and its arguments.
    pub fn new<I, S>(argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
        }
    }

    /// The executable, when the argv is not empty.
    pub fn program(&self) -> Option<&str> {
        self.argv.first().map(String::as_str)
    }
}

/// A WebSocket endpoint to dial.
#[derive(Clone, PartialEq, Eq)]
pub struct WsSpec {
    /// The `ws://` or `wss://` URL the host configured.
    pub url: String,
    /// A bearer token the host chose to present, when the endpoint requires one.
    ///
    /// The library never reads, stores or derives this: it is a value the host passes in for one
    /// dial, and it is sent as an `Authorization` header and nowhere else. A vendor login is not
    /// a source for it — the library does not handle logins.
    pub bearer: Option<String>,
}

impl fmt::Debug for WsSpec {
    /// Hand-written, because this is the one spec that holds a credential.
    ///
    /// A derived `Debug` prints the bearer in the clear, and `TransportSpec` derives its own from
    /// this one — so a `tracing::debug!(?spec)`, or the `received {spec:?}` idiom the crate's own
    /// assertions use, would put the token in a log. The URL goes through the same redaction a
    /// stderr tail does, for the `wss://user:password@host` form.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WsSpec")
            .field("url", &crate::redact::stderr_text(&self.url))
            .field("bearer", &self.bearer.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl WsSpec {
    /// Dials this URL with no credential.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer: None,
        }
    }

    /// Presents this bearer token on the dial.
    #[must_use]
    pub fn with_bearer(mut self, bearer: impl Into<String>) -> Self {
        self.bearer = Some(bearer.into());
        self
    }
}

/// Which carrier the official Agent Client Protocol client rides.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AcpSpec {
    /// A child process's pipes, spawned through the host's launcher.
    ChildPipes(StdioSpec),
    /// An HTTP endpoint the host configured.
    ///
    /// A URL, and a URL is a place a credential hides: `https://svc:password@agent.internal` is
    /// what a host configures when the endpoint is behind basic auth. It is redacted in
    /// [`Debug`] for the same reason [`WsSpec`]'s is.
    Http(String),
}

impl fmt::Debug for AcpSpec {
    /// Hand-written, because the HTTP arm holds a URL that may carry a password.
    ///
    /// A derived `Debug` prints the userinfo in the clear, and `TransportSpec` derives its own
    /// from this one — so a `tracing::debug!(?spec)`, or the `received {spec:?}` idiom the crate's
    /// own assertions use, would put it in a log.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChildPipes(spec) => formatter.debug_tuple("ChildPipes").field(spec).finish(),
            Self::Http(url) => formatter
                .debug_tuple("Http")
                .field(&crate::redact::stderr_text(url))
                .finish(),
        }
    }
}

/// Where a harness executable was found, when the host resolved one.
///
/// The host owns resolution: it knows the toolchain, the version manager and the sandbox the
/// child will run under. The library never searches `PATH` on its own initiative.
///
/// Carried on [`OpenSession`](crate::OpenSession) rather than on the host, because a path is
/// resolved for one harness: a host that resolved Claude and then opened a Codex session through
/// the same context would otherwise have spawned the Claude binary with Codex's arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ExecutablePath(Option<PathBuf>);

impl ExecutablePath {
    /// The path the host resolved, if any.
    pub fn get(&self) -> Option<&PathBuf> {
        self.0.as_ref()
    }

    /// Uses this path instead of the bare program name.
    pub fn resolved(path: impl Into<PathBuf>) -> Self {
        Self(Some(path.into()))
    }

    /// Falls back to `program`, which the launcher resolves however it likes.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::ExecutablePath;
    ///
    /// assert_eq!(ExecutablePath::default().or(String::from("claude")), "claude");
    /// assert_eq!(
    ///     ExecutablePath::resolved("/opt/claude/bin/claude").or(String::from("claude")),
    ///     "/opt/claude/bin/claude"
    /// );
    /// ```
    pub fn or(&self, program: String) -> String {
        match &self.0 {
            Some(path) => path.to_string_lossy().into_owned(),
            None => program,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AcpSpec, ExecutablePath, StdioSpec, TransportKind, TransportSpec, WsSpec};

    #[test]
    fn every_spec_names_its_kind() {
        let cases = [
            (
                TransportSpec::Stdio(StdioSpec::new(["codex", "app-server"])),
                TransportKind::Stdio,
            ),
            (
                TransportSpec::WebSocket(WsSpec::new("wss://localhost:1455")),
                TransportKind::WebSocket,
            ),
            (
                TransportSpec::Acp(AcpSpec::Http(String::from("https://localhost:8080"))),
                TransportKind::Acp,
            ),
        ];
        for (spec, expected) in cases {
            assert_eq!(
                spec.kind(),
                expected,
                "expected {expected}, received {spec:?}"
            );
        }
    }

    #[test]
    fn a_stdio_spec_names_its_program() {
        let spec = StdioSpec::new(["claude", "-p", "--output-format", "stream-json"]);
        assert_eq!(spec.program(), Some("claude"));
        assert_eq!(spec.argv.len(), 4);
        assert_eq!(StdioSpec::new(Vec::<String>::new()).program(), None);
    }

    #[test]
    fn a_websocket_spec_carries_a_bearer_only_when_the_host_passed_one() {
        assert_eq!(WsSpec::new("wss://localhost").bearer, None);
        assert_eq!(
            WsSpec::new("wss://localhost").with_bearer("t").bearer,
            Some(String::from("t"))
        );
    }

    /// The ACP HTTP arm is a URL like the WebSocket one, so it leaks the same password through the
    /// same `{spec:?}` idiom unless it is redacted the same way.
    #[test]
    fn an_acp_endpoints_password_never_reaches_a_debug_line() {
        let spec = AcpSpec::Http(String::from("https://svc:s3cr3t@agent.internal/acp"));
        let rendered = format!("{spec:?}");

        assert!(
            !rendered.contains("s3cr3t"),
            "expected no url password, received {rendered}"
        );
        assert!(
            rendered.contains("agent.internal"),
            "expected the endpoint to stay legible, received {rendered}"
        );

        // `TransportSpec` derives its own `Debug` from this one, so the same must hold there.
        let wrapped = format!("{:?}", TransportSpec::Acp(spec));
        assert!(
            !wrapped.contains("s3cr3t"),
            "expected no url password through the wrapper, received {wrapped}"
        );
    }

    #[test]
    fn a_resolved_executable_wins_over_the_bare_program_name() {
        assert_eq!(ExecutablePath::default().or(String::from("codex")), "codex");
        assert_eq!(
            ExecutablePath::resolved("/usr/local/bin/codex").or(String::from("codex")),
            "/usr/local/bin/codex"
        );
    }
}
