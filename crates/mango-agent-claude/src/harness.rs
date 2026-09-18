//! The Claude Code harness: what it can do, what this machine's build can do, and opening a
//! session against it.
//!
//! Three probes answer "what is here": `claude --version`, `claude --help` and
//! `claude auth status`. All three are documented, read-only and non-secret. Nothing else is read
//! — no credential file, no `~/.claude` anything — except the administrator-managed settings
//! document, which is policy rather than a secret and has to be known *before* a mode is chosen.

use std::sync::Arc;

use mango_external_agents::{
    AuthState, Capabilities, CapabilityCeiling, Configuration, ConfigurationCatalog,
    ConfigurationState, Discovery, Dispatch, Error, ExecutablePath, GateVerdict, Harness,
    HarnessDescriptor, HarnessIdentity, HostContext, OpenSession, PermissionMatrix, Result,
    Session, SessionCapabilities, SessionIds, SessionSnapshot, SessionState, TransportKind,
    TransportSelection,
};

use crate::auth::{self, Authentication};
use crate::cli_surface::CliSurface;
use crate::mcp::ConfigFile;
use crate::permissions::{self, ModeAvailability};
use crate::pinned::{self, MINIMUM_VERSION, VENDOR, VENDOR_ENVIRONMENT_KEYS};
use crate::probe;
use crate::session::ClaudeSession;
use crate::{argv, models, version};

/// What this harness could support given a new enough CLI.
///
/// The two probed flags are the ceiling too: nothing in [`probed_capabilities`] can exceed what a
/// build could in principle advertise. Every other flag here is a measured verdict rather than an
/// unimplemented stub; see `docs/harness-claude.md` for what was probed and when.
const CEILING: Capabilities = Capabilities {
    model_catalog: true,
    mcp_passthrough: true,
    ..probed_capabilities()
};

/// Claude Code, driven through its documented headless surface.
///
/// Stateless and shareable: it holds no session, caches no discovery and spawns nothing of its own.
/// One instance serves every session a host opens.
pub struct ClaudeHarness {
    descriptor: HarnessDescriptor,
    executable: ExecutablePath,
}

impl Default for ClaudeHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeHarness {
    /// A harness that lets the host's launcher resolve the program name.
    ///
    /// The library never searches `PATH` on its own initiative: the host knows the toolchain, the
    /// version manager and the sandbox the child will run under.
    pub fn new() -> Self {
        Self {
            descriptor: HarnessDescriptor {
                identity: HarnessIdentity::claude(),
                vendor: VENDOR,
                capabilities: CapabilityCeiling::new(CEILING),
                transports: &[TransportKind::Stdio],
                vendor_environment_keys: VENDOR_ENVIRONMENT_KEYS,
            },
            executable: ExecutablePath::default(),
        }
    }

    /// A harness that spawns this executable rather than the bare program name.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_claude::ClaudeHarness;
    ///
    /// let harness = ClaudeHarness::new().with_executable("/opt/claude/bin/claude");
    /// ```
    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<std::path::PathBuf>) -> Self {
        self.executable = ExecutablePath::resolved(executable);
        self
    }

    /// The executable this session should spawn: the host's per-request answer, or the harness's.
    fn executable_for(&self, request: &OpenSession) -> ExecutablePath {
        match request.executable.get() {
            Some(path) => ExecutablePath::resolved(path.clone()),
            None => self.executable.clone(),
        }
    }

    /// Everything the three probes established, in one pass.
    async fn survey(&self, host: &HostContext, executable: &ExecutablePath) -> Survey {
        let Some(banner) = probe::output(host, executable, &["--version"]).await else {
            return Survey::default();
        };
        let version = version::parse(&banner);
        let surface = probe::output(host, executable, &["--help"])
            .await
            .map(|help| CliSurface::parse(&help))
            .filter(CliSurface::is_usable);

        // A build that cannot be driven is not asked who is signed in: the answer would be true and
        // useless, and it costs a third process launch to learn.
        if let Some(refusal) = CliSurface::refusal(surface.as_ref(), version.as_ref()) {
            return Survey {
                banner: Some(banner),
                version,
                refusal: Some(refusal),
                ..Survey::default()
            };
        }

        // Neither read depends on the other's result: one is a process boot, the other a file read.
        let (authentication, auto_mode_disabled_by_policy) = tokio::join!(
            async {
                probe::output(host, executable, &["auth", "status"])
                    .await
                    .map_or_else(Authentication::unknown, |stdout| {
                        auth::parse_status(&stdout)
                    })
            },
            read_auto_mode_policy(host)
        );
        let availability = ModeAvailability {
            account_kind: authentication.kind,
            auto_mode_disabled_by_policy,
            accepted_modes: surface
                .as_ref()
                .and_then(CliSurface::accepted_modes)
                .cloned(),
        };

        Survey {
            banner: Some(banner),
            version,
            refusal: None,
            authentication,
            availability,
            surface,
        }
    }

    /// Reuses a host-vouched probe where its public record is sufficient, while re-reading the
    /// help grammar this harness must parse to build safe argv.
    async fn survey_for_open(
        &self,
        host: &HostContext,
        executable: &ExecutablePath,
        receipt: Option<&mango_external_agents::DiscoveryReceipt>,
    ) -> Survey {
        let Some(receipt) = receipt else {
            return self.survey(host, executable).await;
        };

        let surface = probe::output(host, executable, &["--help"])
            .await
            .map(|help| CliSurface::parse(&help))
            .filter(CliSurface::is_usable);
        let version = receipt
            .discovery
            .version
            .as_deref()
            .and_then(version::parse);
        let refusal = match receipt.discovery.gate {
            GateVerdict::NotInstalled | GateVerdict::VersionTooOld { .. } => Some(String::new()),
            GateVerdict::Usable | GateVerdict::Unknown => {
                CliSurface::refusal(surface.as_ref(), version.as_ref())
            }
            _ => Some(String::new()),
        };
        let account_kind = match receipt.discovery.auth {
            AuthState::LoggedIn {
                mode: mango_external_agents::AuthMode::Subscription,
            } => Some(crate::auth::AccountKind::Subscription),
            AuthState::LoggedIn {
                mode: mango_external_agents::AuthMode::ApiKey,
            } => Some(crate::auth::AccountKind::ApiKey),
            AuthState::LoggedIn {
                mode: mango_external_agents::AuthMode::Other(_),
            } => Some(crate::auth::AccountKind::CloudProvider),
            AuthState::LoggedOut { .. } | AuthState::Unknown => None,
            _ => None,
        };
        let availability = ModeAvailability {
            account_kind,
            auto_mode_disabled_by_policy: read_auto_mode_policy(host).await,
            accepted_modes: surface
                .as_ref()
                .and_then(CliSurface::accepted_modes)
                .cloned(),
        };

        Survey {
            banner: receipt
                .discovery
                .version
                .clone()
                .or_else(|| Some(String::from("receipt"))),
            version,
            refusal,
            authentication: Authentication {
                state: receipt.discovery.auth.clone(),
                kind: account_kind,
            },
            availability,
            surface,
        }
    }
}

/// What the probes found, before it is shaped into a [`Discovery`] or a session.
#[derive(Default)]
struct Survey {
    banner: Option<String>,
    version: Option<semver::Version>,
    /// Why this build cannot be driven, when it cannot.
    refusal: Option<String>,
    authentication: Authentication,
    availability: ModeAvailability,
    surface: Option<CliSurface>,
}

impl Survey {
    /// Whether `--version` reported anything at all.
    fn installed(&self) -> bool {
        self.banner.is_some()
    }

    /// The one line of `--version` output a host should be shown.
    ///
    /// `probe::output` hands back every line the CLI printed joined together, because a build can
    /// arrive behind a wrapper script that prints its own preamble first and
    /// [`version::parse`](crate::version::parse) scans all of it for the token that is a version.
    /// What a host renders is a single line, so only one travels: the line the version was read
    /// from when there is one, and otherwise the first line that said anything. A `version` field
    /// carrying a whole multi-line stdout is not a version, and neither is a
    /// [`GateVerdict`](mango_external_agents::GateVerdict) that quotes one back at the person
    /// holding the binary.
    fn reported(&self) -> Option<String> {
        let banner = self.banner.as_deref()?;
        let lines = || {
            banner
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
        };
        lines()
            .find(|line| self.version.is_some() && version::parse(line) == self.version)
            .or_else(|| lines().next())
            .map(str::to_owned)
    }

    /// The version as a host should read it: what the CLI reported, bounded by the core.
    fn found(&self) -> String {
        self.version
            .as_ref()
            .map(semver::Version::to_string)
            .or_else(|| self.reported())
            .unwrap_or_else(|| String::from("an unreadable version"))
    }

    /// Whether this build declares `--mcp-config` on its help surface.
    fn declares_mcp_config(&self) -> bool {
        self.surface
            .as_ref()
            .is_some_and(CliSurface::declares_mcp_config)
    }

    /// What this probed build can do, given whether it advertises a model catalog.
    fn capabilities(&self, model_catalog: bool) -> Capabilities {
        Capabilities {
            model_catalog,
            mcp_passthrough: self.declares_mcp_config(),
            ..probed_capabilities()
        }
    }
}

#[async_trait::async_trait]
impl Harness for ClaudeHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    /// The upper bound before a build, account or managed policy narrows the choices.
    ///
    /// Declaring auto-review here does not enable it. Discovery and session opening still require
    /// an eligible account and a CLI that advertises the mode.
    fn permission_matrix(&self) -> PermissionMatrix {
        permissions::matrix(&ModeAvailability {
            account_kind: Some(crate::auth::AccountKind::Subscription),
            ..ModeAvailability::default()
        })
    }

    async fn probe(&self, host: &HostContext) -> Result<Discovery> {
        let survey = self.survey(host, &self.executable).await;
        if !survey.installed() {
            return Ok(Discovery::not_installed());
        }

        let executable = self.executable.get().cloned();
        if survey.refusal.is_some() {
            return Ok(Discovery {
                executable,
                version: survey.reported(),
                gate: GateVerdict::VersionTooOld {
                    found: survey.found(),
                    minimum: String::from(MINIMUM_VERSION),
                },
                auth: AuthState::Unknown,
                capabilities: Capabilities::none().into(),
                permission_matrix: permissions::matrix(&survey.availability),
                models: Vec::new(),
                // Claude does not publish a settings catalog over any documented surface — see
                // `docs/harness-claude.md`. Empty is the honest answer, distinct from a catalog
                // whose rows are all unsupported.
                configuration_catalog: ConfigurationCatalog::empty(),
            });
        }

        let models = models::catalog(survey.surface.as_ref());
        Ok(Discovery {
            executable,
            version: survey.reported(),
            // The surface said every flag a turn passes is there. A version this harness could not
            // read is not a reason to refuse a binary that answered every other question.
            gate: GateVerdict::Usable,
            auth: survey.authentication.state.clone(),
            capabilities: survey.capabilities(models.is_some()).into(),
            permission_matrix: permissions::matrix(&survey.availability),
            models: models.unwrap_or_default(),
            configuration_catalog: ConfigurationCatalog::empty(),
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        self.validate_open_session(host, &request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        // Claude's model and permission-mode flags are argv on a fresh child: there is no surface
        // to un-set one on an already-open session, so a reset is refused wherever a host could
        // ask for one rather than silently treated as "leave it alone".
        mango_external_agents::configuration::refuse_unsupported_native(&request.configuration)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        mango_external_agents::configuration::refuse_unsupported_reset(&request.configuration)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        Self::refuse_unsupported_fallback(&request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;

        // This is a host authorization check, not a fact a vendor can answer. Prepare the file
        // before the first probe so a missing or inaccessible scratch location never starts a
        // vendor process. The host owns the container or sandbox mapping that makes this path
        // visible to the child. A later opening refusal drops this value and removes only its
        // scoped artifact.
        // Held in `Prepared` rather than as a bare value: three child processes are awaited before
        // the session can take it, and a caller that cancels in between drops it on the worker.
        let mut mcp_config = crate::mcp::Prepared::new(if request.mcp_servers.is_empty() {
            None
        } else {
            let scratch = host.scratch().ok_or_else(|| Error::HostConfiguration {
                expected: "a host-owned scratch directory for MCP configuration",
                received: String::from("none"),
            })?;
            ConfigFile::write(&request.mcp_servers, scratch).await?
        });
        let executable = self.executable_for(&request);
        let survey = self
            .survey_for_open(host, &executable, request.discovery.as_ref())
            .await;

        // Every refusal below owns the artifact written above, and removing it is a synchronous
        // `remove_dir_all` against the host's own scratch root. Gathered into one call so a failed
        // open has one error path, and that path hands the removal to the blocking pool for the
        // same reason the write runs there.
        let opened = match Self::opened(request, survey, self.descriptor(), host.now()) {
            Ok(opened) => opened,
            Err(error) => {
                crate::mcp::release_off_worker(mcp_config.take()).await;
                return Err(error);
            }
        };

        Ok(Box::new(ClaudeSession::new(
            host.clone(),
            executable,
            SessionState::new(std::sync::Arc::clone(host.clock()), opened.snapshot),
            opened.availability,
            opened.surface,
            opened.resume_pending,
            mcp_config.take(),
        )))
    }
}

/// What a passed open decided, before the session takes ownership of its MCP artifact.
struct OpenedSession {
    snapshot: SessionSnapshot,
    availability: ModeAvailability,
    surface: Option<CliSurface>,
    /// A strict resume's candidate handle, confirmed only after `system/init`.
    resume_pending: bool,
}

impl ClaudeHarness {
    /// Every check between the probe and the session, in the order a refusal should reach a host.
    ///
    /// Separated from `open_session` so that call owns exactly one error path: the MCP artifact is
    /// already on disk by the time any of these run, and each refusal has to release it the same
    /// way.
    fn opened(
        request: OpenSession,
        survey: Survey,
        descriptor: &HarnessDescriptor,
        observed_at: std::time::SystemTime,
    ) -> Result<OpenedSession> {
        if !survey.installed() {
            return Err(Error::Launch {
                program: String::from(probe::PROGRAM),
                message: String::from("a CLI that reported no version"),
            });
        }
        if survey.refusal.is_some() {
            return Err(Error::VersionGate {
                found: survey.found(),
                minimum: String::from(MINIMUM_VERSION),
            });
        }
        if let AuthState::LoggedOut { login_hint } = &survey.authentication.state {
            return Err(Error::AuthRequired {
                login_hint: login_hint.clone(),
            });
        }

        let opening_configuration = request.configuration.requested();
        require_supported(&opening_configuration, &survey.availability)?;
        models::validate_configuration(&opening_configuration, survey.surface.as_ref())?;

        // Refused rather than dropped. A session that quietly ignored the servers a host
        // configured would run every turn without the tools somebody set up, and report success.
        if !request.mcp_servers.is_empty() && !survey.declares_mcp_config() {
            return Err(Error::not_supported(
                mango_external_agents::Capability::McpPassthrough,
            ));
        }
        let resume_pending = request.resume.is_some();
        let native_session_id = match &request.resume {
            // Vetted rather than taken on trust, and before anything touches the disk. The
            // reference goes on the command line as `--resume <value>`, and a stored one
            // beginning with `-` would be read by the CLI's parser as a flag rather than as the
            // option's value — the same argument-injection seam `models::safe_model` closes for
            // `--model`. Refused rather than dropped: a resume this harness silently ignored
            // would start a new conversation under the name of the one the host meant to continue.
            Some(resume) if !argv::is_vendor_session_id(&resume.native_session_id) => {
                return Err(Error::HostConfiguration {
                    expected: "a resume reference shaped like the UUID Claude Code mints",
                    received: argv::value_summary(&resume.native_session_id),
                });
            }
            Some(resume) => resume.native_session_id.clone(),
            // `--session-id` takes a UUID and nothing else, so the handle is minted here rather
            // than derived from the host's own session id, which has no shape requirement.
            None => uuid::Uuid::new_v4().to_string(),
        };

        let effective_transport = descriptor.resolve_transport(request.transport)?;
        let capabilities = survey.capabilities(models::advertises_catalog(survey.surface.as_ref()));

        // Requested only, not accepted: opening starts nothing — see this crate's own module
        // documentation — so nothing has actually been encoded onto an argv yet. The first turn
        // is what accepts it; see `ClaudeSession::start_turn`. Observed starts unknown: a later
        // `system/init` can report the model a live Claude process selected.
        let configuration_state = ConfigurationState::new(
            opening_configuration,
            Configuration::unknown(),
            Configuration::unknown(),
        );

        let snapshot = SessionSnapshot::opening(
            SessionIds {
                session_id: request.session_id,
                native_session_id,
            },
            HarnessIdentity::claude(),
            TransportSelection::new(request.transport, effective_transport),
            observed_at,
        )
        .with_capabilities(SessionCapabilities::new(capabilities))
        .with_configuration(configuration_state)
        .with_catalog(ConfigurationCatalog::empty());
        Ok(OpenedSession {
            snapshot,
            availability: survey.availability,
            surface: survey.surface,
            resume_pending,
        })
    }

    /// Refuses fallback before any probe or temporary MCP artifact is created.
    ///
    /// Claude's documented headless mode has no separate resume operation or typed
    /// cannot-resume result. Retrying a failed first turn under a new id could hide an auth,
    /// transport or acceptance-unknown failure as a fresh conversation, so only strict resume is
    /// sound until the vendor publishes a conclusive signal.
    fn refuse_unsupported_fallback(request: &OpenSession) -> Result<()> {
        if request
            .resume
            .as_ref()
            .is_none_or(|resume| resume.mode != mango_external_agents::ResumeMode::Fallback)
        {
            return Ok(());
        }
        Err(Error::HostConfiguration {
            expected: "a strict Claude resume; the documented headless surface does not provide a conclusive cannot-resume signal for fallback",
            received: String::from("fallback resume"),
        })
    }
}

/// What a build that passed the gate offers, before the two per-install flags are filled in.
///
/// Each `false` below is measured. `interactive_approvals`: the CLI has a real control channel and
/// it is not reachable from a documented surface — see `docs/harness-claude.md`. `steering`:
/// `--input-format stream-json` accepts a second message, but it runs as its own turn with its own
/// result, which is a queued follow-up rather than same-turn steering. `session_listing`: the
/// vendor's own transcripts live under a path documented as subject to change, and parsing them
/// would be reading another company's private format. `images`, `native_review` and
/// `account_usage`: no surface observed. `questions`: no documented surface distinguishes an
/// information request from a permission prompt. `configuration_catalog`: no documented surface
/// enumerates the vendor's own settings, only accepts flags this harness already knows about.
/// `session_configuration`: every setting here is argv on a fresh child, so nothing can change on
/// an already-open session.
const fn probed_capabilities() -> Capabilities {
    Capabilities {
        structured_streaming: true,
        reasoning_stream: true,
        resume: true,
        cancellation: true,
        usage_reporting: true,
        configuration: true,
        model_catalog: false,
        mcp_passthrough: false,
        interactive_approvals: false,
        questions: false,
        images: false,
        steering: false,
        session_listing: false,
        native_review: false,
        account_usage: false,
        configuration_catalog: false,
        session_configuration: false,
    }
}

/// Refuses a pair this account and this build cannot run, before anything is spawned.
fn require_supported(configuration: &Configuration, availability: &ModeAvailability) -> Result<()> {
    permissions::configuration_mode(configuration, availability).map(drop)
}

/// `disableAutoMode` from the administrator-managed settings document.
///
/// Never inferred from a failed run: the CLI rejects `--permission-mode auto` *at startup* when
/// policy forbids it, and a startup rejection is indistinguishable from any other startup failure.
/// A missing file is the common case and an unreadable one is not a policy statement, so either way
/// `auto` stays decided by the account.
async fn read_auto_mode_policy(host: &HostContext) -> bool {
    let path = pinned::managed_settings_path(std::env::consts::OS, host.environment());
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .map(|settings| permissions::auto_mode_disabled(&settings))
        .unwrap_or(false)
}

/// The registry entry a host adds to drive Claude Code.
///
/// # Example
///
/// ```
/// use mango_agent_claude::harness;
/// use mango_external_agents::{HarnessId, HarnessRegistry};
///
/// let registry = HarnessRegistry::new(vec![harness()]).expect("expected a registry");
/// assert!(registry.get(&HarnessId::claude()).is_some());
/// ```
pub fn harness() -> Arc<dyn Harness> {
    Arc::new(ClaudeHarness::new())
}

#[cfg(test)]
mod tests {
    use super::{CEILING, ClaudeHarness, probed_capabilities, require_supported};
    use mango_external_agents::{
        ApprovalRouting, Capabilities, Configuration, Error, Harness, HarnessId, PermissionLevel,
        TransportKind,
    };

    #[test]
    fn the_ceiling_covers_everything_a_probe_can_report() {
        assert!(
            probed_capabilities().within(&CEILING),
            "expected the probed set inside the declared ceiling"
        );
        let richest = Capabilities {
            model_catalog: true,
            mcp_passthrough: true,
            ..probed_capabilities()
        };
        assert!(
            richest.within(&CEILING),
            "received {:?}",
            richest.beyond(&CEILING)
        );
    }

    #[test]
    fn declares_only_the_transport_this_dialect_rides() {
        let harness = ClaudeHarness::new();
        let descriptor = harness.descriptor();
        assert_eq!(descriptor.id(), &HarnessId::claude());
        assert_eq!(descriptor.transports, &[TransportKind::Stdio]);

        let error = descriptor
            .require_transport(&TransportKind::Acp)
            .expect_err("expected an undeclared transport to be refused");
        assert!(
            matches!(error, Error::UnsupportedTransport { .. }),
            "received {error:?}"
        );
    }

    #[test]
    fn the_declared_matrix_is_a_ceiling_for_all_six_cells() {
        let matrix = ClaudeHarness::new().permission_matrix();
        assert_eq!(matrix.cells().len(), 6);
        assert!(
            matrix.supports(PermissionLevel::Default, ApprovalRouting::User),
            "expected the ordinary pair to be selectable before any probe"
        );
        assert!(
            matrix.supports(PermissionLevel::Default, ApprovalRouting::AutoReview),
            "expected the declaration to retain modes an eligible account can use"
        );
    }

    #[test]
    fn a_configuration_no_probe_vetted_is_refused_before_anything_is_spawned() {
        let availability = crate::permissions::ModeAvailability::default();
        let auto_review = Configuration::unknown()
            .with_level(PermissionLevel::Default)
            .with_routing(ApprovalRouting::AutoReview);
        let error = require_supported(&auto_review, &availability)
            .expect_err("expected an unverified auto to be refused");
        assert!(
            matches!(error, Error::HostConfiguration { .. }),
            "received {error:?}"
        );

        require_supported(&Configuration::unknown(), &availability)
            .expect("expected an omitted vendor-default configuration to need no probe verdict");
    }
}
