//! One of the two axes: which vendor dialect a harness speaks, and what it can do.
//!
//! A [`HarnessDescriptor`] is a fact about the crate, knowable without touching the machine: who
//! the vendor is, which transport kinds the harness accepts, which environment variables it
//! documents, and the ceiling of what it could ever support. What *this* machine's installed CLI
//! supports is a different question, answered by [`Discovery`](crate::Discovery) per probe.

use std::fmt;

/// Which vendor dialect a harness speaks.
///
/// The transport is the other axis and is chosen separately: a harness declares the transport
/// kinds it accepts in [`HarnessDescriptor::transports`], and an unsupported pair is refused with
/// [`Error::UnsupportedTransport`](crate::Error::UnsupportedTransport) before anything is spawned.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum HarnessKind {
    /// Claude Code, through its documented headless stream-json mode.
    Claude,
    /// OpenAI Codex, through `codex app-server`.
    Codex,
    /// Any Agent Client Protocol agent, under the named profile.
    Acp(AcpProfileId),
}

impl fmt::Display for HarnessKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => formatter.write_str("claude"),
            Self::Codex => formatter.write_str("codex"),
            Self::Acp(profile) => write!(formatter, "acp:{profile}"),
        }
    }
}

/// Which ACP agent a generic Agent Client Protocol harness was configured for.
///
/// A profile is the argv and the quirks of one agent — `cursor`, `opencode`, `gemini`, `goose`,
/// the vendor shims — or `custom`, where the host supplies the argv itself. It is a plain string
/// rather than an enum because the set is open: an ACP agent nobody has heard of yet speaks the
/// same protocol as the ones that ship with a profile.
///
/// # Example
///
/// ```
/// use mango_external_agents::{AcpProfileId, HarnessKind};
///
/// let kind = HarnessKind::Acp(AcpProfileId::new("cursor"));
/// assert_eq!(kind.to_string(), "acp:cursor");
/// ```
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AcpProfileId(String);

impl AcpProfileId {
    /// Names a profile.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The profile as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AcpProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Who owns the CLI a harness drives, as data.
///
/// The company and the two documents are what a host's disclosure has to name, and a host that
/// paraphrased a vendor's terms would be making a claim about another company's obligations. The
/// library carries the links and never summarises them. The URLs stay locale-free on purpose:
/// these sites redirect a bare path to the reader's own language, so the un-prefixed link is the
/// one that serves a `pt-BR` reader Portuguese.
///
/// Branding is nominative use only: the name identifies the tool being launched. No logos, no
/// wordmarks, nothing implying an official or endorsed integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorInfo {
    /// The company behind the CLI, which is who the terms are with.
    pub company: &'static str,
    /// The consumer-facing terms, which is what a subscription-backed sign-in is under.
    pub terms_url: &'static str,
    /// The vendor's privacy policy.
    pub privacy_url: &'static str,
    /// Whether this vendor's own skills double as `/name` slash commands.
    ///
    /// Probed against each CLI rather than inferred from docs: Claude Code and Cursor list every
    /// skill under `/` in their own catalog; Codex reads skills into a prompt section instead, so
    /// offering one as a slash command would advertise a command the CLI never registers.
    pub skills_are_slash_commands: bool,
}

/// What a harness or an installed CLI can do.
///
/// Nothing is true by default. A flag is a refusal to fake parity: the descriptor states the
/// ceiling this crate could ever reach, and a [`Discovery`](crate::Discovery) states what the
/// build on this machine actually offers. Discovery may report less than the ceiling — an old CLI
/// meeting a new harness — but never more, which
/// [`Capabilities::within`] checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// A parseable event stream, not a text transcript to scrape.
    pub structured_streaming: bool,
    /// Reasoning is streamed as its own events rather than folded into the answer.
    pub reasoning_stream: bool,
    /// A real request/response approval exchange, not a prompt written to a TTY.
    pub interactive_approvals: bool,
    /// A previous session can be resumed by its native id.
    pub resume: bool,
    /// The vendor enumerates the models it will accept.
    pub model_catalog: bool,
    /// Image attachments reach the vendor.
    pub images: bool,
    /// Token usage is reported.
    pub usage_reporting: bool,
    /// A running turn can be cancelled.
    pub cancellation: bool,
    /// Same-turn steering, not a queued follow-up message.
    pub steering: bool,
    /// The vendor's own sessions can be listed.
    pub session_listing: bool,
    /// The vendor runs a review of its own.
    pub native_review: bool,
    /// Account-level plan quota can be read.
    pub account_usage: bool,
    /// The vendor accepts MCP servers the host configured, passed through untouched.
    pub mcp_passthrough: bool,
}

impl Capabilities {
    /// Every capability, for a harness whose ceiling is the whole table.
    pub const fn all() -> Self {
        Self {
            structured_streaming: true,
            reasoning_stream: true,
            interactive_approvals: true,
            resume: true,
            model_catalog: true,
            images: true,
            usage_reporting: true,
            cancellation: true,
            steering: true,
            session_listing: true,
            native_review: true,
            account_usage: true,
            mcp_passthrough: true,
        }
    }

    /// None, which is the only honest answer before a probe has run.
    pub const fn none() -> Self {
        Self {
            structured_streaming: false,
            reasoning_stream: false,
            interactive_approvals: false,
            resume: false,
            model_catalog: false,
            images: false,
            usage_reporting: false,
            cancellation: false,
            steering: false,
            session_listing: false,
            native_review: false,
            account_usage: false,
            mcp_passthrough: false,
        }
    }

    /// Whether an optional method's capability is set.
    pub const fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Steering => self.steering,
            Capability::SessionListing => self.session_listing,
            Capability::NativeReview => self.native_review,
            Capability::AccountUsage => self.account_usage,
        }
    }

    /// Every flag this set claims that `ceiling` does not.
    ///
    /// One direction only, deliberately. A harness that implements `steer` may still meet a CLI
    /// build that cannot steer and has to report `steering: false`; advertising what cannot be
    /// called is the bug.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::Capabilities;
    ///
    /// let ceiling = Capabilities { steering: true, ..Capabilities::none() };
    /// let probed = Capabilities { steering: true, native_review: true, ..Capabilities::none() };
    /// assert_eq!(probed.beyond(&ceiling), vec!["nativeReview"]);
    /// ```
    pub fn beyond(&self, ceiling: &Self) -> Vec<&'static str> {
        let pairs: [(&'static str, bool, bool); 13] = [
            (
                "structuredStreaming",
                self.structured_streaming,
                ceiling.structured_streaming,
            ),
            (
                "reasoningStream",
                self.reasoning_stream,
                ceiling.reasoning_stream,
            ),
            (
                "interactiveApprovals",
                self.interactive_approvals,
                ceiling.interactive_approvals,
            ),
            ("resume", self.resume, ceiling.resume),
            ("modelCatalog", self.model_catalog, ceiling.model_catalog),
            ("images", self.images, ceiling.images),
            (
                "usageReporting",
                self.usage_reporting,
                ceiling.usage_reporting,
            ),
            ("cancellation", self.cancellation, ceiling.cancellation),
            ("steering", self.steering, ceiling.steering),
            (
                "sessionListing",
                self.session_listing,
                ceiling.session_listing,
            ),
            ("nativeReview", self.native_review, ceiling.native_review),
            ("accountUsage", self.account_usage, ceiling.account_usage),
            (
                "mcpPassthrough",
                self.mcp_passthrough,
                ceiling.mcp_passthrough,
            ),
        ];
        pairs
            .into_iter()
            .filter(|(_, claimed, allowed)| *claimed && !*allowed)
            .map(|(name, _, _)| name)
            .collect()
    }

    /// Whether this set stays inside `ceiling`.
    pub fn within(&self, ceiling: &Self) -> bool {
        self.beyond(ceiling).is_empty()
    }
}

/// An optional method a harness may or may not implement.
///
/// The four that [`Session`](crate::Session) leaves to a default returning
/// [`Error::NotSupported`](crate::Error::NotSupported).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Capability {
    /// [`Session::steer`](crate::Session::steer).
    Steering,
    /// [`Session::list_sessions`](crate::Session::list_sessions).
    SessionListing,
    /// [`Session::start_review`](crate::Session::start_review).
    NativeReview,
    /// [`Session::refresh_account_usage`](crate::Session::refresh_account_usage).
    AccountUsage,
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Steering => "steering",
            Self::SessionListing => "session listing",
            Self::NativeReview => "native review",
            Self::AccountUsage => "account usage",
        })
    }
}

/// Everything about a harness that is true before anything is spawned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessDescriptor {
    /// Which vendor dialect this harness speaks.
    pub kind: HarnessKind,
    /// Who owns the CLI, and the documents a host's disclosure links.
    pub vendor: VendorInfo,
    /// The ceiling: what this harness could support given a new enough CLI.
    pub capabilities: Capabilities,
    /// The transport kinds this harness accepts, in preference order.
    pub transports: &'static [crate::transport::TransportKind],
    /// Vendor-documented environment variables that survive the allowlist.
    ///
    /// Named by the harness, never by a host request: a host cannot use this seam to smuggle a
    /// credential of its own into a vendor child.
    pub vendor_environment_keys: &'static [&'static str],
}

impl HarnessDescriptor {
    /// Whether this harness declares `transport`.
    pub fn supports_transport(&self, transport: &crate::transport::TransportKind) -> bool {
        self.transports.contains(transport)
    }

    /// Refuses a (harness, transport) pair the harness does not declare.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedTransport`](crate::Error::UnsupportedTransport) when the harness does
    /// not accept this transport kind.
    pub fn require_transport(
        &self,
        transport: &crate::transport::TransportKind,
    ) -> crate::Result<()> {
        if self.supports_transport(transport) {
            return Ok(());
        }
        Err(crate::Error::UnsupportedTransport {
            harness: self.kind.clone(),
            transport: *transport,
        })
    }
}

/// One vendor dialect, as a host drives it.
///
/// Stateless and shareable: a harness holds no session, caches no discovery and spawns nothing of
/// its own. Everything it needs arrives in the [`HostContext`](crate::HostContext) it is handed,
/// which is what lets one harness serve every session a host opens.
#[async_trait::async_trait]
pub trait Harness: Send + Sync {
    /// What is true about this harness before anything is spawned.
    fn descriptor(&self) -> &HarnessDescriptor;

    /// Which (level, routing) pairs this harness can run, and why not for the rest.
    fn permission_matrix(&self) -> crate::permission::PermissionMatrix;

    /// What a probe found, bounded before a host sees it.
    ///
    /// This is the method a host calls, and it is provided rather than implemented: it applies
    /// [`Discovery::normalized`](crate::Discovery::normalized) to whatever
    /// [`probe`](Self::probe) returned. Bounding a harness has to remember is bounding one harness
    /// will forget, and what it forgot is a vendor-supplied model id on its way into a picker and
    /// back out as the choice.
    ///
    /// # Errors
    ///
    /// Whatever the probe hit.
    async fn discover(&self, host: &crate::HostContext) -> crate::Result<crate::Discovery> {
        Ok(self.probe(host).await?.normalized())
    }

    /// Probes the machine.
    ///
    /// Never cached here: how fresh an answer has to be is the host's decision, and a harness that
    /// memoised would be making it.
    ///
    /// # Errors
    ///
    /// Whatever the probe hit. A CLI that is simply absent is
    /// [`Discovery::not_installed`](crate::Discovery::not_installed) rather than an error.
    async fn probe(&self, host: &crate::HostContext) -> crate::Result<crate::Discovery>;

    /// Opens a session.
    ///
    /// # Errors
    ///
    /// [`Error::AuthRequired`](crate::Error::AuthRequired) when nobody is signed in,
    /// [`Error::VersionGate`](crate::Error::VersionGate) when the build is too old, and whatever
    /// the vendor or the link reported otherwise.
    async fn open_session(
        &self,
        host: &crate::HostContext,
        request: crate::session::OpenSession,
    ) -> crate::Result<Box<dyn crate::session::Session>>;
}

#[cfg(test)]
mod tests {
    use super::{
        AcpProfileId, Capabilities, Capability, Harness, HarnessDescriptor, HarnessKind, VendorInfo,
    };
    use crate::Error;
    use crate::transport::TransportKind;

    /// A harness that reports what a vendor said and bounds none of it, which is what a harness
    /// author writes when the bounding is somebody else's job to remember.
    struct UnboundedProbe {
        descriptor: HarnessDescriptor,
    }

    impl UnboundedProbe {
        fn new() -> Self {
            Self {
                descriptor: HarnessDescriptor {
                    kind: HarnessKind::Claude,
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: Capabilities::none(),
                    transports: &[TransportKind::Stdio],
                    vendor_environment_keys: &[],
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl Harness for UnboundedProbe {
        fn descriptor(&self) -> &HarnessDescriptor {
            &self.descriptor
        }

        fn permission_matrix(&self) -> crate::permission::PermissionMatrix {
            crate::permission::PermissionMatrix::none(
                crate::permission::UnsupportedReason::NotOfferedByVendor,
            )
        }

        async fn probe(&self, _host: &crate::HostContext) -> crate::Result<crate::Discovery> {
            Ok(crate::Discovery {
                models: vec![
                    crate::Model::new("gpt\u{202e}5-mini"),
                    crate::Model::new("opus"),
                ],
                ..crate::Discovery::not_installed()
            })
        }

        async fn open_session(
            &self,
            _host: &crate::HostContext,
            _request: crate::session::OpenSession,
        ) -> crate::Result<Box<dyn crate::session::Session>> {
            Err(Error::Closed { subject: "session" })
        }
    }

    /// The bounding is the trait's, not the harness author's: `discover` is what a host calls, and
    /// it applies it to whatever `probe` returned.
    #[tokio::test]
    async fn discovering_bounds_what_the_probe_did_not() {
        let host = crate::HostContext::builder()
            .launcher(std::sync::Arc::new(crate::testing::FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("test", "0.0.0")
            .build()
            .expect("expected a host");

        let discovery = UnboundedProbe::new()
            .discover(&host)
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["opus"],
            "expected the unbounded id to be dropped by the trait, not by the harness"
        );
    }

    const VENDOR: VendorInfo = VendorInfo {
        company: "Anthropic",
        terms_url: "https://www.anthropic.com/legal/consumer-terms",
        privacy_url: "https://www.anthropic.com/legal/privacy",
        skills_are_slash_commands: true,
    };

    fn descriptor() -> HarnessDescriptor {
        HarnessDescriptor {
            kind: HarnessKind::Claude,
            vendor: VENDOR,
            capabilities: Capabilities {
                structured_streaming: true,
                ..Capabilities::none()
            },
            transports: &[TransportKind::Stdio],
            vendor_environment_keys: &["CLAUDE_CONFIG_DIR"],
        }
    }

    #[test]
    fn a_harness_kind_prints_its_profile() {
        assert_eq!(HarnessKind::Codex.to_string(), "codex");
        assert_eq!(
            HarnessKind::Acp(AcpProfileId::new("opencode")).to_string(),
            "acp:opencode"
        );
    }

    #[test]
    fn an_undeclared_transport_is_refused_before_anything_is_spawned() {
        let error = descriptor()
            .require_transport(&TransportKind::WebSocket)
            .expect_err("expected a refusal, received acceptance");
        assert!(
            matches!(
                error,
                Error::UnsupportedTransport {
                    harness: HarnessKind::Claude,
                    transport: TransportKind::WebSocket
                }
            ),
            "expected UnsupportedTransport, received {error:?}"
        );
    }

    #[test]
    fn a_declared_transport_is_accepted() {
        descriptor()
            .require_transport(&TransportKind::Stdio)
            .expect("expected stdio to be accepted, received a refusal");
    }

    #[test]
    fn probed_capabilities_may_be_narrower_than_the_ceiling_but_never_wider() {
        let ceiling = Capabilities::all();
        let narrower = Capabilities {
            steering: true,
            ..Capabilities::none()
        };
        assert!(narrower.within(&ceiling));

        let no_steering = Capabilities {
            steering: false,
            ..Capabilities::all()
        };
        assert_eq!(Capabilities::all().beyond(&no_steering), vec!["steering"]);
        assert!(!Capabilities::all().within(&no_steering));
    }

    #[test]
    fn every_optional_capability_reads_from_the_table() {
        let capabilities = Capabilities {
            steering: true,
            session_listing: false,
            native_review: true,
            account_usage: false,
            ..Capabilities::none()
        };
        assert!(capabilities.has(Capability::Steering));
        assert!(!capabilities.has(Capability::SessionListing));
        assert!(capabilities.has(Capability::NativeReview));
        assert!(!capabilities.has(Capability::AccountUsage));
    }
}
