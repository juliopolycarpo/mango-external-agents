//! One of the two axes: which vendor dialect a harness speaks, and what it can do.
//!
//! A [`HarnessDescriptor`] is a fact about the crate, knowable without touching the machine: who
//! the harness is, who the vendor is, which transport kinds it accepts, which environment
//! variables it documents, and the ceiling of what it could ever support. What *this* machine's
//! installed CLI supports is a different question, answered by [`Discovery`](crate::Discovery) per
//! probe, and what one open session ended up with is a third.
//!
//! Those three answers are three types — [`CapabilityCeiling`], [`DiscoveredCapabilities`] and
//! [`SessionCapabilities`] — rather than three values of one. They only ever narrow, in that
//! order, and making the direction a type means a call that passed them the wrong way round does
//! not compile instead of quietly advertising something no build can do.

use std::fmt;

use crate::identity::{HarnessId, HarnessIdentity};

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
/// Nothing is true by default. A flag is a refusal to fake parity. This is the raw table; which
/// of the three questions a given table is answering is carried by [`CapabilityCeiling`],
/// [`DiscoveredCapabilities`] or [`SessionCapabilities`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// A parseable event stream, not a text transcript to scrape.
    pub structured_streaming: bool,
    /// Reasoning is streamed as its own events rather than folded into the answer.
    pub reasoning_stream: bool,
    /// A real request/response approval exchange, not a prompt written to a TTY.
    pub interactive_approvals: bool,
    /// The vendor can stop and ask for information, distinctly from asking for permission.
    pub questions: bool,
    /// A previous session can be resumed by its native id.
    pub resume: bool,
    /// The vendor enumerates the models it will accept.
    pub model_catalog: bool,
    /// The vendor enumerates its configurable options, not just its models.
    pub configuration_catalog: bool,
    /// Settings can be changed on an open session, outside any turn.
    pub session_configuration: bool,
    /// Image attachments reach the vendor.
    pub images: bool,
    /// Token usage is reported.
    pub usage_reporting: bool,
    /// A running turn can be cancelled.
    pub cancellation: bool,
    /// Same-turn steering, not a queued follow-up message.
    pub steering: bool,
    /// The vendor's own sessions can be listed through an open session.
    /// This does not promise the optional harness-level listing service.
    pub session_listing: bool,
    /// The vendor runs a review of its own.
    pub native_review: bool,
    /// Account-level plan quota can be read through an open session.
    /// This does not promise the optional harness-level account service.
    pub account_usage: bool,
    /// The vendor accepts MCP servers the host configured, passed through untouched.
    pub mcp_passthrough: bool,
    /// Explicit configuration can apply to one turn without opening a new session.
    pub configuration: bool,
}

impl Capabilities {
    /// Every capability, for a harness whose ceiling is the whole table.
    pub const fn all() -> Self {
        Self {
            structured_streaming: true,
            reasoning_stream: true,
            interactive_approvals: true,
            questions: true,
            resume: true,
            model_catalog: true,
            configuration_catalog: true,
            session_configuration: true,
            images: true,
            usage_reporting: true,
            cancellation: true,
            steering: true,
            session_listing: true,
            native_review: true,
            account_usage: true,
            mcp_passthrough: true,
            configuration: true,
        }
    }

    /// None, which is the only honest answer before a probe has run.
    pub const fn none() -> Self {
        Self {
            structured_streaming: false,
            reasoning_stream: false,
            interactive_approvals: false,
            questions: false,
            resume: false,
            model_catalog: false,
            configuration_catalog: false,
            session_configuration: false,
            images: false,
            usage_reporting: false,
            cancellation: false,
            steering: false,
            session_listing: false,
            native_review: false,
            account_usage: false,
            mcp_passthrough: false,
            configuration: false,
        }
    }

    /// Whether an optional method's capability is set.
    pub const fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Steering => self.steering,
            Capability::SessionListing => self.session_listing,
            Capability::NativeReview => self.native_review,
            Capability::AccountUsage => self.account_usage,
            Capability::Resume => self.resume,
            Capability::Configuration => self.configuration,
            Capability::SessionConfiguration => self.session_configuration,
            Capability::InteractiveApprovals => self.interactive_approvals,
            Capability::Questions => self.questions,
            Capability::Images => self.images,
            Capability::McpPassthrough => self.mcp_passthrough,
        }
    }

    /// Refuses a request for a capability this session or harness did not advertise.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`](crate::Error::NotSupported) when `capability` is false.
    pub fn require(&self, capability: Capability) -> crate::Result<()> {
        if self.has(capability) {
            return Ok(());
        }
        Err(crate::Error::not_supported(capability))
    }

    /// Removes every capability not present in `ceiling`.
    #[must_use]
    pub const fn clamped_to(self, ceiling: &Self) -> Self {
        Self {
            structured_streaming: self.structured_streaming && ceiling.structured_streaming,
            reasoning_stream: self.reasoning_stream && ceiling.reasoning_stream,
            interactive_approvals: self.interactive_approvals && ceiling.interactive_approvals,
            questions: self.questions && ceiling.questions,
            resume: self.resume && ceiling.resume,
            model_catalog: self.model_catalog && ceiling.model_catalog,
            configuration_catalog: self.configuration_catalog && ceiling.configuration_catalog,
            session_configuration: self.session_configuration && ceiling.session_configuration,
            images: self.images && ceiling.images,
            usage_reporting: self.usage_reporting && ceiling.usage_reporting,
            cancellation: self.cancellation && ceiling.cancellation,
            steering: self.steering && ceiling.steering,
            session_listing: self.session_listing && ceiling.session_listing,
            native_review: self.native_review && ceiling.native_review,
            account_usage: self.account_usage && ceiling.account_usage,
            mcp_passthrough: self.mcp_passthrough && ceiling.mcp_passthrough,
            configuration: self.configuration && ceiling.configuration,
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
        // Destructured rather than read field by field, and without a `..` rest: a capability
        // added to the struct and forgotten here is a compile error instead of a flag that
        // silently escapes the only check standing between a probe and a claim it cannot honour.
        let Self {
            structured_streaming,
            reasoning_stream,
            interactive_approvals,
            questions,
            resume,
            model_catalog,
            configuration_catalog,
            session_configuration,
            images,
            usage_reporting,
            cancellation,
            steering,
            session_listing,
            native_review,
            account_usage,
            mcp_passthrough,
            configuration,
        } = *self;
        let pairs = [
            (
                "structuredStreaming",
                structured_streaming,
                ceiling.structured_streaming,
            ),
            (
                "reasoningStream",
                reasoning_stream,
                ceiling.reasoning_stream,
            ),
            (
                "interactiveApprovals",
                interactive_approvals,
                ceiling.interactive_approvals,
            ),
            ("questions", questions, ceiling.questions),
            ("resume", resume, ceiling.resume),
            ("modelCatalog", model_catalog, ceiling.model_catalog),
            (
                "configurationCatalog",
                configuration_catalog,
                ceiling.configuration_catalog,
            ),
            (
                "sessionConfiguration",
                session_configuration,
                ceiling.session_configuration,
            ),
            ("images", images, ceiling.images),
            ("usageReporting", usage_reporting, ceiling.usage_reporting),
            ("cancellation", cancellation, ceiling.cancellation),
            ("steering", steering, ceiling.steering),
            ("sessionListing", session_listing, ceiling.session_listing),
            ("nativeReview", native_review, ceiling.native_review),
            ("accountUsage", account_usage, ceiling.account_usage),
            ("mcpPassthrough", mcp_passthrough, ceiling.mcp_passthrough),
            ("configuration", configuration, ceiling.configuration),
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

/// The three tiers share a body; only the direction each may be narrowed in differs.
macro_rules! capability_tier {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Capabilities);

        impl $name {
            /// Names this tier's table.
            pub const fn new(capabilities: Capabilities) -> Self {
                Self(capabilities)
            }

            /// Every capability.
            pub const fn all() -> Self {
                Self(Capabilities::all())
            }

            /// None, which is the only honest answer before anything has been established.
            pub const fn none() -> Self {
                Self(Capabilities::none())
            }

            /// The table itself.
            pub const fn capabilities(&self) -> &Capabilities {
                &self.0
            }

            /// Whether one optional method's capability is set.
            pub const fn has(&self, capability: Capability) -> bool {
                self.0.has(capability)
            }

            /// Refuses a request for a capability this tier did not advertise.
            ///
            /// # Errors
            ///
            /// [`Error::NotSupported`](crate::Error::NotSupported) when `capability` is false.
            pub fn require(&self, capability: Capability) -> crate::Result<()> {
                self.0.require(capability)
            }
        }

        impl From<Capabilities> for $name {
            fn from(capabilities: Capabilities) -> Self {
                Self(capabilities)
            }
        }
    };
}

capability_tier!(
    CapabilityCeiling,
    r#"What a harness could support given a new enough CLI.

A fact about the crate, declared before anything is spawned. Nothing narrows it and nothing may
exceed it.

# Example

```
use mango_external_agents::{Capabilities, CapabilityCeiling};

let ceiling = CapabilityCeiling::new(Capabilities { resume: true, ..Capabilities::none() });
assert!(ceiling.capabilities().resume);
```"#
);

capability_tier!(
    DiscoveredCapabilities,
    r#"What the build on this machine actually offers.

Narrower than the [`CapabilityCeiling`] or equal to it, never wider —
[`DiscoveredCapabilities::clamped_to`] is the only way to build one from a probe, and it takes the
ceiling as its own type so the two cannot be passed the wrong way round.

# Example

```
use mango_external_agents::{Capabilities, CapabilityCeiling, DiscoveredCapabilities};

let ceiling = CapabilityCeiling::new(Capabilities { resume: true, ..Capabilities::none() });
let probed = DiscoveredCapabilities::new(Capabilities::all()).clamped_to(&ceiling);
assert!(probed.capabilities().resume);
assert!(!probed.capabilities().steering);
```"#
);

capability_tier!(
    SessionCapabilities,
    r#"What one open session ended up with.

Narrower than what discovery found, or equal to it. A protocol with a handshake learns things at
open time that no probe could: an ACP agent that advertised `session/load` in its manifest and
then negotiated it away is a session that cannot resume, whatever the probe said.

# Example

```
use mango_external_agents::{Capabilities, DiscoveredCapabilities, SessionCapabilities};

let discovered = DiscoveredCapabilities::new(Capabilities { resume: true, ..Capabilities::none() });
let negotiated = SessionCapabilities::all().narrowed_to(&discovered);
assert!(negotiated.capabilities().resume);
assert!(!negotiated.capabilities().images);
```"#
);

impl DiscoveredCapabilities {
    /// This probe's reading with everything the harness never promised removed.
    ///
    /// A probe learns facts from one installed build. The descriptor is the harness's hard limit,
    /// so a host must never receive a claim from the probe that the harness cannot honour.
    #[must_use]
    pub const fn clamped_to(self, ceiling: &CapabilityCeiling) -> Self {
        Self(self.0.clamped_to(&ceiling.0))
    }

    /// Every flag this probe claims that the harness never declared.
    pub fn beyond(&self, ceiling: &CapabilityCeiling) -> Vec<&'static str> {
        self.0.beyond(&ceiling.0)
    }

    /// Whether this probe's reading stays inside what the harness declared.
    pub fn within(&self, ceiling: &CapabilityCeiling) -> bool {
        self.0.within(&ceiling.0)
    }
}

impl SessionCapabilities {
    /// This session's reading with everything the probe did not find removed.
    #[must_use]
    pub const fn narrowed_to(self, discovered: &DiscoveredCapabilities) -> Self {
        Self(self.0.clamped_to(&discovered.0))
    }

    /// Every flag this session claims that discovery did not find.
    pub fn beyond(&self, discovered: &DiscoveredCapabilities) -> Vec<&'static str> {
        self.0.beyond(&discovered.0)
    }

    /// Whether this session's reading stays inside what discovery found.
    pub fn within(&self, discovered: &DiscoveredCapabilities) -> bool {
        self.0.within(&discovered.0)
    }
}

/// An optional method a harness may or may not implement.
///
/// The ones [`Session`](crate::Session) and [`Harness`] leave to a default returning
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
    /// Also names refusals from the independently optional [`Harness::list_sessions`].
    SessionListing,
    /// [`Session::start_review`](crate::Session::start_review).
    NativeReview,
    /// [`Session::refresh_account_usage`](crate::Session::refresh_account_usage).
    /// Also names refusals from the independently optional [`Harness::account_usage`].
    AccountUsage,
    /// [`OpenSession::resuming`](crate::OpenSession::resuming).
    Resume,
    /// [`TurnRequest::with_configuration`](crate::TurnRequest::with_configuration).
    Configuration,
    /// [`Session::configure`](crate::Session::configure).
    SessionConfiguration,
    /// [`Session::respond`](crate::Session::respond).
    InteractiveApprovals,
    /// [`Session::answer`](crate::Session::answer).
    Questions,
    /// Image attachments on [`TurnRequest`](crate::TurnRequest).
    Images,
    /// [`OpenSession::with_mcp_servers`](crate::OpenSession::with_mcp_servers).
    McpPassthrough,
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Steering => "steering",
            Self::SessionListing => "session listing",
            Self::NativeReview => "native review",
            Self::AccountUsage => "account usage",
            Self::Resume => "resume",
            Self::Configuration => "per-turn configuration",
            Self::SessionConfiguration => "mid-session configuration",
            Self::InteractiveApprovals => "interactive approvals",
            Self::Questions => "questions",
            Self::Images => "images",
            Self::McpPassthrough => "MCP passthrough",
        })
    }
}

/// Everything about a harness that is true before anything is spawned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessDescriptor {
    /// Who this harness is: its registration id, its protocol family and its profile.
    pub identity: HarnessIdentity,
    /// Who owns the CLI, and the documents a host's disclosure links.
    pub vendor: VendorInfo,
    /// The ceiling: what this harness could support given a new enough CLI.
    pub capabilities: CapabilityCeiling,
    /// The transport kinds this harness accepts, in preference order.
    pub transports: &'static [crate::transport::TransportKind],
    /// Vendor-documented environment variables that survive the allowlist.
    ///
    /// Named by the harness, never by a host request: a host cannot use this seam to smuggle a
    /// credential of its own into a vendor child.
    pub vendor_environment_keys: &'static [&'static str],
}

impl HarnessDescriptor {
    /// The id this harness registers and dispatches under.
    pub fn id(&self) -> &HarnessId {
        &self.identity.id
    }

    /// Whether this harness declares `transport`.
    pub fn supports_transport(&self, transport: &crate::transport::TransportKind) -> bool {
        self.transports.contains(transport)
    }

    /// The transport this harness uses when a host does not choose one.
    ///
    /// The first it declares, because [`HarnessDescriptor::transports`] is in preference order.
    pub fn default_transport(&self) -> Option<crate::transport::TransportKind> {
        self.transports.first().copied()
    }

    /// Which transport a session will really run on, given what the host asked for.
    ///
    /// The requested kind when the harness accepts it, this harness's default when the host did
    /// not choose — and a refusal rather than a substitution when the two disagree. Silently
    /// running a different carrier than the one a host asked for is how a host ends up debugging
    /// the wrong connection.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedTransport`](crate::Error::UnsupportedTransport) when the harness does
    /// not accept the requested kind, and
    /// [`Error::HostConfiguration`](crate::Error::HostConfiguration) when it declares none at all.
    pub fn resolve_transport(
        &self,
        requested: Option<crate::transport::TransportKind>,
    ) -> crate::Result<crate::transport::TransportKind> {
        match requested {
            Some(transport) => {
                self.require_transport(&transport)?;
                Ok(transport)
            }
            None => self
                .default_transport()
                .ok_or_else(|| crate::Error::HostConfiguration {
                    expected: "a harness declaring at least one transport",
                    received: format!("{} declaring none", self.identity.id),
                }),
        }
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
            harness: self.identity.id.clone(),
            transport: *transport,
        })
    }
}

/// One vendor dialect, as a host drives it.
///
/// Stateless and shareable: a harness holds no session, caches no discovery and spawns nothing of
/// its own. Everything it needs arrives in the [`HostContext`](crate::HostContext) it is handed,
/// which is what lets one harness serve every session a host opens.
///
/// The listing and account methods are deliberately here as well as on
/// [`Session`](crate::Session). A host that wants to show somebody their existing conversations
/// before they pick one should not have to open a conversation first.
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
        Ok(self
            .probe(host)
            .await?
            .normalized()
            .bounded_by(&self.descriptor().capabilities, &self.permission_matrix()))
    }

    /// Refuses session options outside this harness's declared capability ceiling.
    ///
    /// Call this before launching. A probe may narrow the declaration for one installed build, but
    /// it cannot make a harness accept a feature its descriptor never promised.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`](crate::Error::NotSupported) for undeclared strict resume,
    /// [`Error::UnsupportedTransport`](crate::Error::UnsupportedTransport) for a transport the
    /// harness does not accept, or
    /// [`Error::HostConfiguration`](crate::Error::HostConfiguration) with the supplied server count
    /// when host-supplied MCP servers are unsupported.
    fn validate_open_session(
        &self,
        host: &crate::HostContext,
        request: &crate::OpenSession,
    ) -> crate::Result<()> {
        self.descriptor().resolve_transport(request.transport)?;
        if request
            .resume
            .as_ref()
            .is_some_and(|resume| resume.mode == crate::ResumeMode::Strict)
        {
            self.descriptor().capabilities.require(Capability::Resume)?;
        }
        if !request.mcp_servers.is_empty()
            && !self
                .descriptor()
                .capabilities
                .has(Capability::McpPassthrough)
        {
            return Err(crate::Error::HostConfiguration {
                expected: "no MCP servers for a harness without MCP passthrough",
                received: format!("MCP server count {}", request.mcp_servers.len()),
            });
        }
        if let Some(receipt) = &request.discovery {
            // The host's clock, not the process's: a receipt's freshness is a wall-clock question,
            // and reading `SystemTime::now()` here would be this crate going behind the host's
            // back for the one value it was handed a `Clock` to supply — and would put a
            // freshness test beyond the reach of `FrozenClock`.
            receipt.verify_for(self.descriptor(), host.now(), request)?;
        }
        Ok(())
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

    /// Lists the vendor's own sessions without opening a conversation, bounded before a host sees
    /// them.
    ///
    /// What a host calls to populate a picker. Like [`Harness::discover`], it is the half that
    /// applies [`SessionPage::normalized`](crate::SessionPage::normalized), so a title or a
    /// workspace path a vendor wrote cannot reach a host's list unbounded.
    /// A session-listing capability does not guarantee this separate harness-level service.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`](crate::Error::NotSupported) unless the harness implements
    /// [`list_native_sessions`](Self::list_native_sessions).
    async fn list_sessions(
        &self,
        host: &crate::HostContext,
        query: crate::session::SessionQuery,
    ) -> crate::Result<crate::session::SessionPage> {
        Ok(self
            .list_native_sessions(host, query.normalized())
            .await?
            .normalized())
    }

    /// The page as the vendor returned it. Implemented by a harness that supports listing.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`](crate::Error::NotSupported) unless the harness implements it.
    async fn list_native_sessions(
        &self,
        _host: &crate::HostContext,
        _query: crate::session::SessionQuery,
    ) -> crate::Result<crate::session::SessionPage> {
        Err(crate::Error::not_supported(Capability::SessionListing))
    }

    /// Reads account-level plan quota without opening a conversation.
    ///
    /// Reported from a non-secret vendor surface, like everything else about an account here. The
    /// library still has no login of any kind.
    /// A session account-usage capability does not guarantee this separate harness-level service.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`](crate::Error::NotSupported) unless the harness implements it.
    async fn account_usage(
        &self,
        _host: &crate::HostContext,
    ) -> crate::Result<crate::session::AccountUsage> {
        Err(crate::Error::not_supported(Capability::AccountUsage))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Capabilities, Capability, CapabilityCeiling, DiscoveredCapabilities, Harness,
        HarnessDescriptor, SessionCapabilities, VendorInfo,
    };
    use crate::Error;
    use crate::identity::{HarnessIdentity, ProfileId};
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
                    identity: HarnessIdentity::claude(),
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: CapabilityCeiling::none(),
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
                capabilities: DiscoveredCapabilities::all(),
                permission_matrix: crate::permission::PermissionMatrix::build(|_, _| {
                    crate::permission::ConfigurationVerdict::supported()
                }),
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
        assert_eq!(
            discovery.capabilities,
            DiscoveredCapabilities::none(),
            "expected the probe's wider capability claim to be clamped to the descriptor"
        );
        assert!(
            discovery
                .permission_matrix
                .cells()
                .iter()
                .all(|cell| !cell.supported),
            "expected a probe to be unable to widen the declared permission matrix"
        );
    }

    /// A harness-level listing must not need a conversation first: a host populating a picker has
    /// nothing to open one on.
    #[tokio::test]
    async fn listing_is_callable_without_a_live_session_and_refuses_when_unimplemented() {
        let host = crate::HostContext::builder()
            .launcher(std::sync::Arc::new(crate::testing::FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("test", "0.0.0")
            .build()
            .expect("expected a host");

        let error = UnboundedProbe::new()
            .list_sessions(&host, crate::SessionQuery::default())
            .await
            .expect_err("expected a typed refusal, received a page");
        assert!(
            matches!(
                error,
                Error::NotSupported {
                    capability: Capability::SessionListing
                }
            ),
            "expected an unsupported listing, received {error:?}"
        );

        let error = UnboundedProbe::new()
            .account_usage(&host)
            .await
            .expect_err("expected a typed refusal, received a reading");
        assert!(
            matches!(
                error,
                Error::NotSupported {
                    capability: Capability::AccountUsage
                }
            ),
            "expected unsupported account usage, received {error:?}"
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
            identity: HarnessIdentity::claude(),
            vendor: VENDOR,
            capabilities: CapabilityCeiling::new(Capabilities {
                structured_streaming: true,
                ..Capabilities::none()
            }),
            transports: &[TransportKind::Stdio],
            vendor_environment_keys: &["CLAUDE_CONFIG_DIR"],
        }
    }

    #[test]
    fn a_harness_identity_prints_the_id_a_host_persists() {
        assert_eq!(HarnessIdentity::codex().to_string(), "codex");
        assert_eq!(
            HarnessIdentity::acp(ProfileId::new("opencode").expect("a profile")).to_string(),
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
                    transport: TransportKind::WebSocket,
                    ..
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

    /// Silently running a different carrier than the one a host asked for is how a host ends up
    /// debugging the wrong connection, so the resolution refuses rather than substitutes.
    #[test]
    fn a_requested_transport_is_honoured_or_refused_and_never_substituted() {
        let descriptor = descriptor();
        assert_eq!(
            descriptor
                .resolve_transport(Some(TransportKind::Stdio))
                .expect("expected the requested transport"),
            TransportKind::Stdio
        );
        assert_eq!(
            descriptor
                .resolve_transport(None)
                .expect("expected the declared default"),
            TransportKind::Stdio
        );
        assert!(
            descriptor
                .resolve_transport(Some(TransportKind::WebSocket))
                .is_err()
        );
    }

    fn test_host() -> crate::HostContext {
        crate::HostContext::builder()
            .launcher(std::sync::Arc::new(crate::testing::FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("test", "0.0.0")
            .build()
            .expect("expected a host")
    }

    #[test]
    fn fallback_resume_reaches_a_harness_that_does_not_declare_resume() {
        let harness = UnboundedProbe::new();
        let request =
            crate::OpenSession::new("chat-1").resuming("native-1", crate::ResumeMode::Fallback);

        harness
            .validate_open_session(&test_host(), &request)
            .expect("expected fallback resume to reach the harness implementation");
    }

    #[test]
    fn rejected_mcp_configuration_reports_the_received_server_count() {
        let harness = UnboundedProbe::new();
        let request = crate::OpenSession::new("chat-1").with_mcp_servers(vec![
            crate::McpServer::stdio("one", "first-mcp"),
            crate::McpServer::stdio("two", "second-mcp"),
        ]);
        let error = harness
            .validate_open_session(&test_host(), &request)
            .expect_err("unsupported MCP servers");
        assert!(
            error.to_string().contains("received MCP server count 2"),
            "expected the rejected MCP server count in the diagnostic, received {error}"
        );
    }

    /// The freshness comparison reads the host's clock, so a host that froze time can write a test
    /// about a stale receipt without waiting for wall-clock seconds to pass.
    #[test]
    fn a_receipt_is_aged_against_the_hosts_own_clock() {
        use crate::testing::FrozenClock;
        use std::time::Duration;

        let clock = std::sync::Arc::new(FrozenClock::default());
        let host = crate::HostContext::builder()
            .launcher(std::sync::Arc::new(crate::testing::FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("test", "0.0.0")
            .clock(clock.clone())
            .build()
            .expect("expected a host");

        let receipt = crate::DiscoveryReceipt::new(
            crate::HarnessId::claude(),
            crate::Discovery::not_installed(),
            host.now(),
        )
        .valid_for(Duration::from_secs(60));
        let request = crate::OpenSession::new("chat-1").with_discovery(receipt);
        let harness = UnboundedProbe::new();

        harness
            .validate_open_session(&host, &request)
            .expect("expected a fresh receipt to be accepted");

        clock.advance(Duration::from_secs(61));
        let error = harness
            .validate_open_session(&host, &request)
            .expect_err("expected a receipt past its window to be refused");
        assert!(
            error.to_string().contains("61s old, valid for 60s"),
            "expected the age and the window in the diagnostic, received {error}"
        );
    }

    /// The three tiers narrow in one direction, and the types are what make it impossible to pass
    /// them the other way round.
    #[test]
    fn each_tier_narrows_towards_the_next_and_never_widens() {
        let ceiling = CapabilityCeiling::new(Capabilities {
            steering: true,
            resume: true,
            ..Capabilities::none()
        });
        let discovered = DiscoveredCapabilities::all().clamped_to(&ceiling);
        assert!(discovered.capabilities().steering);
        assert!(!discovered.capabilities().images);
        assert!(discovered.within(&ceiling));

        let negotiated = SessionCapabilities::new(Capabilities {
            steering: true,
            resume: true,
            images: true,
            ..Capabilities::none()
        })
        .narrowed_to(&discovered);
        assert!(
            !negotiated.capabilities().images,
            "expected a session to be unable to claim what the probe never found"
        );
        assert!(negotiated.within(&discovered));
        assert_eq!(
            SessionCapabilities::all().beyond(&discovered).len(),
            Capabilities::all().beyond(discovered.capabilities()).len()
        );
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
        assert!(!capabilities.has(Capability::Resume));
        assert!(!capabilities.has(Capability::Configuration));
        assert!(!capabilities.has(Capability::SessionConfiguration));
        assert!(!capabilities.has(Capability::InteractiveApprovals));
        assert!(!capabilities.has(Capability::Questions));
        assert!(!capabilities.has(Capability::Images));
        assert!(!capabilities.has(Capability::McpPassthrough));
        assert!(
            matches!(
                capabilities.require(Capability::Images),
                Err(Error::NotSupported {
                    capability: Capability::Images
                })
            ),
            "expected the unsupported capability itself in the typed refusal"
        );
    }
}
