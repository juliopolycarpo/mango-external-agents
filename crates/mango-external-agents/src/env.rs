//! The positive environment allowlist for vendor processes.
//!
//! A vendor child receives the keys named here plus whatever the harness documents in
//! [`HarnessDescriptor::vendor_environment_keys`](crate::HarnessDescriptor::vendor_environment_keys),
//! and nothing else. The list is positive rather than a denylist because the failure worth
//! preventing is a host's own secret — a connector token, a database URL — reaching a third
//! party's process, and a denylist can only refuse the leaks somebody thought of.
//!
//! There is deliberately no way for a host to pass a map of values through a request: a host
//! cannot use this seam to smuggle a credential into a child even if it wanted to.

use std::collections::BTreeMap;

/// Keys every vendor child gets, whatever the harness.
///
/// Locations and operating-system metadata, never application configuration. The version-manager
/// roots are here because a vendor CLI that shells out to `nvm`/`fnm`/`bun` needs them to agree
/// with the runtime it was started under; the Windows entries are what ordinary process creation
/// needs there.
///
/// `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA` and the four XDG base directories
/// (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`) name where the user's
/// own files live. A CLI resolves its config, auth file and cache from them, so a child that
/// cannot see the ones the user set reads the default location, which can be another account's
/// state rather than a missing one. They are paths, not secrets.
///
/// Deliberately absent: `XDG_RUNTIME_DIR`, which holds the login session's sockets and which
/// Cursor's own CLI strips from an environment it hands on; `XDG_DATA_DIRS` and `XDG_CONFIG_DIRS`,
/// which are system search paths; every token, API key, secret, password, cookie and proxy
/// variable, because a proxy URL can carry userinfo.
pub const BASE_ENVIRONMENT_KEYS: &[&str] = &[
    "PATH",
    "Path",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "NO_COLOR",
    "NVM_DIR",
    "FNM_DIR",
    "BUN_INSTALL",
    "SystemRoot",
    "WINDIR",
    "ComSpec",
    "PATHEXT",
];

/// The environment a host offers, as the values it is willing to pass on.
///
/// A host builds one from its own process, from a toolchain selection it resolved, or from
/// nothing at all. The library reads it and never `std::env::var`s behind the host's back.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct EnvSource(BTreeMap<String, String>);

impl std::fmt::Debug for EnvSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvSource")
            .field("keys", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl EnvSource {
    /// An empty source. A child launched from it sees only what the allowlist can find, which is
    /// nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Everything in this process's environment, for a host that has no narrower answer.
    ///
    /// The allowlist still applies: reading the host's environment here is not the same as
    /// passing it on.
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    /// A source built from explicit pairs.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::EnvSource;
    ///
    /// let source = EnvSource::from_pairs([("PATH", "/bin"), ("CONNECTOR_SECRET", "never")]);
    /// assert_eq!(source.get("PATH"), Some("/bin"));
    /// ```
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }

    /// One value, if the host offered it. On Windows, casing is ignored and the first
    /// spelling in lexical order wins, just as it does in [`allowlist`].
    pub fn get(&self, key: &str) -> Option<&str> {
        if cfg!(windows) {
            return self
                .0
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.as_str());
        }
        self.0.get(key).map(String::as_str)
    }

    /// Every pair the host offered, before the allowlist.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

/// The environment one vendor child actually receives.
///
/// `vendor_keys` comes from the harness's own descriptor — documented variables that vendor's CLI
/// reads — never from a host request. Any `LC_*` key survives on top of the base list, because the
/// locale set is open and a missing one changes how a CLI formats what it prints.
/// Key matching follows the operating system: case-insensitive on Windows, exact on Unix.
/// When Windows sources contain two casings of one key, the first key in lexical order wins.
/// The child receives only that spelling and value, so executable lookup and launch agree.
///
/// # Example
///
/// ```
/// use mango_external_agents::{EnvSource, env};
///
/// let source = EnvSource::from_pairs([
///     ("PATH", "/bin"),
///     ("LC_MESSAGES", "pt_BR.UTF-8"),
///     ("CONNECTOR_SECRET", "never-forward-this"),
///     ("CLAUDE_CONFIG_DIR", "/home/ada/.claude"),
/// ]);
/// let child = env::allowlist(&source, &["CLAUDE_CONFIG_DIR"]);
///
/// assert_eq!(child.get("PATH").map(String::as_str), Some("/bin"));
/// assert_eq!(child.get("LC_MESSAGES").map(String::as_str), Some("pt_BR.UTF-8"));
/// assert_eq!(child.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/home/ada/.claude"));
/// assert_eq!(child.get("CONNECTOR_SECRET"), None);
/// ```
pub fn allowlist(source: &EnvSource, vendor_keys: &[&str]) -> BTreeMap<String, String> {
    allowlist_with_case(source, vendor_keys, cfg!(windows))
}

fn allowlist_with_case(
    source: &EnvSource,
    vendor_keys: &[&str],
    case_insensitive: bool,
) -> BTreeMap<String, String> {
    let allowed = source
        .iter()
        .filter(|(key, _)| is_allowed(key, vendor_keys, case_insensitive))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    if case_insensitive {
        return windows_effective_environment(&allowed);
    }
    allowed
}

/// Gives Windows lookup and process creation one casing and one value for each key.
pub(crate) fn windows_effective_environment(
    environment: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut effective = BTreeMap::new();
    let mut names: Vec<&str> = Vec::new();
    for (name, value) in environment {
        if names.iter().any(|seen| seen.eq_ignore_ascii_case(name)) {
            continue;
        }
        names.push(name);
        effective.insert(name.clone(), value.clone());
    }
    effective
}

fn is_allowed(key: &str, vendor_keys: &[&str], case_insensitive: bool) -> bool {
    if case_insensitive {
        return BASE_ENVIRONMENT_KEYS
            .iter()
            .chain(vendor_keys)
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
            || key
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("LC_"));
    }
    BASE_ENVIRONMENT_KEYS.contains(&key) || vendor_keys.contains(&key) || key.starts_with("LC_")
}

#[cfg(test)]
mod tests {
    use super::{BASE_ENVIRONMENT_KEYS, EnvSource, allowlist, allowlist_with_case};
    use std::collections::BTreeMap;

    fn pairs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn keeps_only_the_positive_base_and_harness_allowlists() {
        let source = EnvSource::from_pairs([
            ("PATH", "/bin"),
            ("HOME", "/home/ada"),
            ("LANG", "en_US.UTF-8"),
            ("LC_MESSAGES", "pt_BR.UTF-8"),
            ("CONNECTOR_SECRET", "never-forward-this"),
            ("VENDOR_CONFIG", "harness-owned"),
            ("NVM_DIR", "/home/ada/.nvm"),
            ("FNM_DIR", "/home/ada/.local/share/fnm"),
            ("BUN_INSTALL", "/home/ada/.bun"),
        ]);

        assert_eq!(
            allowlist(&source, &["VENDOR_CONFIG"]),
            pairs(&[
                ("PATH", "/bin"),
                ("HOME", "/home/ada"),
                ("LANG", "en_US.UTF-8"),
                ("LC_MESSAGES", "pt_BR.UTF-8"),
                ("VENDOR_CONFIG", "harness-owned"),
                ("NVM_DIR", "/home/ada/.nvm"),
                ("FNM_DIR", "/home/ada/.local/share/fnm"),
                ("BUN_INSTALL", "/home/ada/.bun"),
            ])
        );
    }

    #[test]
    fn a_host_secret_never_reaches_a_child_even_with_no_harness_keys() {
        let source = EnvSource::from_pairs([
            ("PATH", "/bin"),
            ("CONNECTOR_SECRET", "never-forward-this"),
            ("DATABASE_URL", "postgres://u:p@db/main"),
        ]);

        let child = allowlist(&source, &[]);
        assert_eq!(child, pairs(&[("PATH", "/bin")]));
    }

    #[test]
    fn a_harness_key_is_not_a_wildcard_for_its_prefix() {
        let source = EnvSource::from_pairs([
            ("CLAUDE_CONFIG_DIR", "/home/ada/.claude"),
            ("CLAUDE_API_KEY", "never-forward-this"),
        ]);

        assert_eq!(
            allowlist(&source, &["CLAUDE_CONFIG_DIR"]),
            pairs(&[("CLAUDE_CONFIG_DIR", "/home/ada/.claude")])
        );
    }

    /// The XDG base directories relocate the user's own config, data, state and cache, exactly as
    /// `HOME` does. A child that cannot see them resolves `~/.config` and reads another account's
    /// state than the shell the user started the host from.
    #[test]
    fn the_xdg_base_directories_reach_every_child() {
        let source = EnvSource::from_pairs([
            ("XDG_CONFIG_HOME", "/home/ada/cfg"),
            ("XDG_DATA_HOME", "/home/ada/data"),
            ("XDG_STATE_HOME", "/home/ada/state"),
            ("XDG_CACHE_HOME", "/home/ada/cache"),
        ]);

        assert_eq!(
            allowlist(&source, &[]),
            pairs(&[
                ("XDG_CONFIG_HOME", "/home/ada/cfg"),
                ("XDG_DATA_HOME", "/home/ada/data"),
                ("XDG_STATE_HOME", "/home/ada/state"),
                ("XDG_CACHE_HOME", "/home/ada/cache"),
            ]),
            "expected the four XDG base directories in the base allowlist | received a child without them"
        );
    }

    /// Cursor resolves its Windows `auth.json` from `APPDATA`, and GitHub documents Copilot's
    /// Windows cache under `LOCALAPPDATA`; both are per-user locations like `USERPROFILE`.
    #[test]
    fn the_windows_per_user_locations_reach_every_child() {
        let source = EnvSource::from_pairs([
            ("APPDATA", r"D:\Users\ada\Roaming"),
            ("LOCALAPPDATA", r"D:\Users\ada\Local"),
        ]);

        assert_eq!(
            allowlist(&source, &[]),
            pairs(&[
                ("APPDATA", r"D:\Users\ada\Roaming"),
                ("LOCALAPPDATA", r"D:\Users\ada\Local"),
            ]),
            "expected APPDATA and LOCALAPPDATA in the base allowlist | received a child without them"
        );
    }

    /// Lookalikes stay out: the session runtime directory holds the login session's sockets, the
    /// search-path lists are not locations of the user's own files, and a proxy URL can carry
    /// userinfo.
    #[test]
    fn xdg_neighbours_proxies_and_credentials_are_still_dropped() {
        let source = EnvSource::from_pairs([
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_DATA_DIRS", "/usr/share"),
            ("XDG_CONFIG_DIRS", "/etc/xdg"),
            ("XDG_CONFIG_HOME_TOKEN", "never-forward-this"),
            ("HTTPS_PROXY", "http://user:pass@proxy:3128"),
            ("ALL_PROXY", "socks5://user:pass@proxy:1080"),
            ("GITHUB_TOKEN", "never-forward-this"),
            ("OPENAI_API_KEY", "never-forward-this"),
            ("PATH", "/bin"),
        ]);

        assert_eq!(
            allowlist(&source, &[]),
            pairs(&[("PATH", "/bin")]),
            "expected only PATH | received a runtime directory, search path, proxy or credential"
        );
    }

    #[test]
    fn windows_matches_the_xdg_base_directories_without_regard_to_case() {
        let source = EnvSource::from_pairs([
            ("xdg_config_home", r"C:\cfg"),
            ("Xdg_Runtime_Dir", r"C:\run"),
        ]);

        assert_eq!(
            allowlist_with_case(&source, &[], true),
            pairs(&[("xdg_config_home", r"C:\cfg")])
        );
        assert!(allowlist_with_case(&source, &[], false).is_empty());
    }

    #[test]
    fn no_base_key_is_credential_bearing() {
        for key in BASE_ENVIRONMENT_KEYS {
            let upper = key.to_ascii_uppercase();
            let forbidden = [
                "TOKEN", "KEY", "SECRET", "PASSWORD", "PROXY", "COOKIE", "AUTH",
            ]
            .iter()
            .find(|word| upper.contains(*word));
            assert_eq!(
                forbidden, None,
                "expected a location or locale key | received credential-shaped {key:?}"
            );
        }
    }

    #[test]
    fn an_empty_source_yields_an_empty_environment() {
        assert!(allowlist(&EnvSource::empty(), &["ANYTHING"]).is_empty());
    }

    #[test]
    fn environment_key_casing_follows_the_host_platform() {
        let source = EnvSource::from_pairs([
            ("SYSTEMROOT", r"C:\Windows"),
            ("COMSPEC", r"C:\Windows\system32\cmd.exe"),
            ("vendor_config", "allowed"),
            ("lc_messages", "en_US.UTF-8"),
            ("VENDOR_CONFIG_SECRET", "forbidden"),
        ]);
        let child = allowlist(&source, &["VENDOR_CONFIG"]);
        if cfg!(windows) {
            assert_eq!(
                child.get("SYSTEMROOT").map(String::as_str),
                Some(r"C:\Windows"),
                "expected Git Bash's uppercase SYSTEMROOT to survive the Windows allowlist"
            );
            assert_eq!(child.len(), 4);
            assert_eq!(
                child.get("vendor_config").map(String::as_str),
                Some("allowed")
            );
            assert!(!child.contains_key("VENDOR_CONFIG_SECRET"));
            return;
        }
        assert!(
            child.is_empty(),
            "expected case-sensitive environment keys on Unix"
        );
    }

    #[test]
    fn windows_key_casing_has_one_effective_value_for_lookup_and_child() {
        let source = EnvSource::from_pairs([
            ("PATH", r"C:\first"),
            ("Path", r"C:\second"),
            ("SYSTEMROOT", r"C:\Windows"),
            ("SystemRoot", r"C:\Other"),
        ]);
        let child = allowlist_with_case(&source, &[], true);
        assert_eq!(child.get("PATH").map(String::as_str), Some(r"C:\first"));
        assert_eq!(child.get("Path"), None);
        assert_eq!(
            child.get("SYSTEMROOT").map(String::as_str),
            Some(r"C:\Windows")
        );
        assert_eq!(child.get("SystemRoot"), None);
        assert_eq!(child.len(), 2);
        if cfg!(windows) {
            assert_eq!(source.get("path"), Some(r"C:\first"));
            assert_eq!(source.get("systemroot"), Some(r"C:\Windows"));
        } else {
            assert_eq!(source.get("path"), None);
        }
    }

    #[test]
    fn windows_single_casing_round_trips_and_unix_keeps_distinct_keys() {
        let source = EnvSource::from_pairs([("Path", r"C:\tools"), ("PATH", r"/usr/bin")]);
        let windows = allowlist_with_case(&source, &[], true);
        assert_eq!(windows, pairs(&[("PATH", "/usr/bin")]));

        let unix = allowlist_with_case(&source, &[], false);
        assert_eq!(unix, pairs(&[("PATH", "/usr/bin"), ("Path", r"C:\tools")]));

        let only_path = EnvSource::from_pairs([("Path", r"C:\tools")]);
        assert_eq!(
            allowlist_with_case(&only_path, &[], true),
            pairs(&[("Path", r"C:\tools")])
        );
    }

    #[test]
    fn debug_shows_offered_keys_without_their_values() {
        let source =
            EnvSource::from_pairs([("CONNECTOR_SECRET", "canary-env-secret"), ("PATH", "/bin")]);
        let debug = format!("{source:?}");
        assert!(debug.contains("CONNECTOR_SECRET"));
        assert!(debug.contains("PATH"));
        assert!(!debug.contains("canary-env-secret"));
        assert!(!debug.contains("/bin"));
    }
}
