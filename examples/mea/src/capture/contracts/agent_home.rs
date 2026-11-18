//! The scratch home an `--open-session` capture runs an adapter under.
//!
//! `session/new` makes an adapter start its backend, which reads the account kept under `HOME`,
//! `CLAUDE_CONFIG_DIR` or `CODEX_HOME` and writes its own state there. A capture that passed the
//! operator's values through would open a session as the operator, write into `~/.claude` or
//! `~/.codex`, and commit a catalog that reflects that account. So the capture builds its own
//! empty directories, hands the adapter only those, and removes them when it is done. That is what
//! makes the capture unauthenticated by construction rather than by the operator's care.

use std::path::Path;

use mango_external_agents::EnvSource;
use mango_external_agents::error::{Error, Result};

/// Every variable that names where an agent finds an account or keeps state. The scratch home
/// overrides each of them, whatever the operator set, and the list is the base allowlist's location
/// keys plus the two vendor homes.
const ISOLATED_KEYS: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
];

/// A temporary, empty home for one capture. Dropping it removes the directories, on every path out
/// of the capture, an error included.
pub(super) struct AgentHome {
    root: tempfile::TempDir,
}

impl AgentHome {
    /// Creates the empty directories every key in [`ISOLATED_KEYS`] will name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::HostConfiguration`] naming the temporary directory root when it cannot be
    /// created.
    pub(super) fn create() -> Result<Self> {
        let parent = std::env::temp_dir();
        let root = tempfile::Builder::new()
            .prefix("mea-agent-home-")
            .tempdir_in(&parent)
            .map_err(|error| Error::HostConfiguration {
                expected: "a writable temporary directory for the capture's scratch home",
                received: format!("{parent:?}: {error}"),
            })?;
        for key in ISOLATED_KEYS {
            let directory = Self::directory_of(root.path(), key);
            std::fs::create_dir_all(&directory).map_err(|error| Error::HostConfiguration {
                expected: "a writable scratch directory inside the capture's scratch home",
                received: format!("{directory:?}: {error}"),
            })?;
        }
        Ok(Self { root })
    }

    /// The directory that holds all of them.
    pub(super) fn root(&self) -> &Path {
        self.root.path()
    }

    fn directory_of(root: &Path, key: &str) -> std::path::PathBuf {
        root.join(key.to_ascii_lowercase())
    }

    /// `base` with every key in [`ISOLATED_KEYS`] pointed into this home; every other pair is kept,
    /// so `PATH`, the locale and the rest of what the allowlist forwards are the operator's.
    pub(super) fn environment(&self, base: &EnvSource) -> EnvSource {
        let isolated: Vec<(String, String)> = ISOLATED_KEYS
            .iter()
            .map(|key| {
                (
                    (*key).to_owned(),
                    Self::directory_of(self.root(), key)
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .collect();
        EnvSource::from_pairs(
            base.iter()
                // Windows matches names without case, so a `Home` of the operator's must go too.
                .filter(|(name, _)| {
                    !ISOLATED_KEYS
                        .iter()
                        .any(|key| name.eq_ignore_ascii_case(key))
                })
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .chain(isolated),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentHome, ISOLATED_KEYS};
    use mango_external_agents::EnvSource;

    /// A named fake of the operator's process environment: a logged-in machine's homes, a `PATH`
    /// and a secret the allowlist would not forward anyway.
    fn operators_environment() -> EnvSource {
        EnvSource::from_pairs([
            ("HOME", "/real/home"),
            ("USERPROFILE", "/real/profile"),
            ("APPDATA", "/real/appdata"),
            ("LOCALAPPDATA", "/real/localappdata"),
            ("XDG_CONFIG_HOME", "/real/xdg/config"),
            ("XDG_DATA_HOME", "/real/xdg/data"),
            ("XDG_STATE_HOME", "/real/xdg/state"),
            ("XDG_CACHE_HOME", "/real/xdg/cache"),
            ("CLAUDE_CONFIG_DIR", "/real/claude"),
            ("CODEX_HOME", "/real/codex"),
            ("PATH", "/fake/bin"),
            ("LANG", "C"),
        ])
    }

    #[test]
    fn every_account_location_points_into_the_scratch_home_and_nothing_else_moves() {
        let home = AgentHome::create().expect("expected a scratch home");

        let environment = home.environment(&operators_environment());

        for key in ISOLATED_KEYS {
            let value = environment
                .get(key)
                .unwrap_or_else(|| panic!("expected {key} to be set in the scratch environment"));
            assert!(
                std::path::Path::new(value).starts_with(home.root()),
                "expected {key} inside {:?} | received {value}",
                home.root()
            );
            assert!(
                !value.starts_with("/real"),
                "expected the operator's {key} replaced | received {value}"
            );
            assert!(
                std::path::Path::new(value).is_dir(),
                "expected {key} to name an existing empty directory | received {value}"
            );
            assert_eq!(
                std::fs::read_dir(value).expect("scratch dir").count(),
                0,
                "expected {key} to name an empty directory"
            );
        }
        assert_eq!(environment.get("PATH"), Some("/fake/bin"));
        assert_eq!(environment.get("LANG"), Some("C"));
    }

    #[test]
    fn a_key_the_operator_did_not_set_is_still_isolated() {
        let home = AgentHome::create().expect("expected a scratch home");

        let environment = home.environment(&EnvSource::from_pairs([("PATH", "/fake/bin")]));

        assert!(
            environment
                .get("CODEX_HOME")
                .is_some_and(|value| value.starts_with(home.root().to_str().expect("utf-8 root"))),
            "expected CODEX_HOME to be set to the scratch home when the operator set none"
        );
    }

    #[test]
    fn dropping_the_home_removes_every_directory() {
        let home = AgentHome::create().expect("expected a scratch home");
        let root = home.root().to_owned();
        std::fs::write(root.join("claude_config_dir/.claude.json"), "{}")
            .expect("state an adapter would write");

        drop(home);

        assert!(
            !root.exists(),
            "expected {root:?} removed with the scratch home"
        );
    }
}
