//! What a built-in profile's child receives from the host's environment.
//!
//! A vendor CLI that keeps its account under `$XDG_CONFIG_HOME` shows the wrong account, or none,
//! when the child never sees the variable. These tests launch each profile through the fake
//! launcher, once as a version probe and once as a session, and compare the child's environment
//! with what the host offered.
use super::*;
use mango_external_agents::{EnvSource, OpenSession};

/// Location variables every child gets, whatever the profile.
const BASE_LOCATIONS: [(&str, &str); 4] = [
    ("XDG_CONFIG_HOME", "/work/xdg/config"),
    ("XDG_DATA_HOME", "/work/xdg/data"),
    ("XDG_STATE_HOME", "/work/xdg/state"),
    ("XDG_CACHE_HOME", "/work/xdg/cache"),
];

/// Variables no child may receive: a session directory, credentials, proxies that can carry
/// userinfo, and the variables that make a process load code it was not shipped with.
const NEVER_FORWARDED: [(&str, &str); 25] = [
    ("LD_PRELOAD", "/work/canary-preload.so"),
    ("LD_LIBRARY_PATH", "/work/canary-libs"),
    ("DYLD_INSERT_LIBRARIES", "/work/canary-insert.dylib"),
    ("DYLD_LIBRARY_PATH", "/work/canary-dylibs"),
    ("NODE_OPTIONS", "--require /work/canary.js"),
    ("BUN_OPTIONS", "--preload /work/canary.js"),
    ("ANTHROPIC_API_KEY", "canary-anthropic-key"),
    ("ANTHROPIC_AUTH_TOKEN", "canary-anthropic-token"),
    ("CLAUDE_CODE_OAUTH_TOKEN", "canary-claude-oauth"),
    ("CODEX_API_KEY", "canary-codex-key"),
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("CURSOR_API_KEY", "canary-cursor-api-key"),
    ("CURSOR_AUTH_TOKEN", "canary-cursor-token"),
    ("XAI_API_KEY", "canary-xai-key"),
    ("OPENAI_API_KEY", "canary-openai-key"),
    ("GITHUB_TOKEN", "canary-github-token"),
    ("COPILOT_GITHUB_TOKEN", "canary-copilot-token"),
    ("GEMINI_API_KEY", "canary-gemini-key"),
    (
        "GOOGLE_APPLICATION_CREDENTIALS",
        "/work/canary-credentials.json",
    ),
    ("GOOSE_PROVIDER__API_KEY", "canary-goose-key"),
    ("OPENCODE_SERVER_PASSWORD", "canary-opencode-password"),
    ("OPENCODE_CONFIG_CONTENT", "{\"canary\":true}"),
    ("CODEX_CONFIG", "{\"canary\":true}"),
    ("HTTPS_PROXY", "http://user:canary-pass@proxy.example:3128"),
    ("ALL_PROXY", "socks5://user:canary-pass@proxy.example:1080"),
];

/// The location variable each vendor documents for relocating its own home, by profile id.
///
/// Each is cited in `docs/harness-acp.md`. The two adapters forward the variable of the CLI they
/// drive, which the Codex and Claude harnesses already forward for the same reason.
const PROFILE_LOCATIONS: [(&str, &str, &str); 8] = [
    ("codex-acp", "CODEX_HOME", "/work/codex"),
    ("claude-agent-acp", "CLAUDE_CONFIG_DIR", "/work/claude"),
    ("cursor", "CURSOR_CONFIG_DIR", "/work/cursor"),
    ("grok", "GROK_HOME", "/work/grok"),
    ("opencode", "OPENCODE_CONFIG_DIR", "/work/opencode"),
    ("gemini", "GEMINI_CLI_HOME", "/work/gemini"),
    ("copilot", "COPILOT_HOME", "/work/copilot"),
    ("goose", "GOOSE_PATH_ROOT", "/work/goose"),
];

fn host_offering(
    launcher: Arc<FakeLauncher>,
    offered: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .environment(EnvSource::from_pairs(offered))
        .client_info("environment-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

/// The environment of the version probe and of the session child, in launch order.
async fn child_environments(
    profile: &str,
    offered: &[(&'static str, &'static str)],
) -> Vec<std::collections::BTreeMap<String, String>> {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        FakeAcpAgent::new()
            .printing_version("1.0.0")
            .version_process(),
    );
    launcher.push(FakeAcpAgent::new().process());
    let context = host_offering(launcher.clone(), offered.iter().copied());
    let harness = AcpHarness::builtin(profile).expect("expected a built-in profile");
    harness
        .discover(&context)
        .await
        .expect("expected discovery");
    let session = harness
        .open_session(&context, OpenSession::new("environment"))
        .await
        .expect("expected the session to open");
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
    launcher
        .launches()
        .into_iter()
        .map(|launch| launch.env)
        .collect()
}

fn offered_everything() -> Vec<(&'static str, &'static str)> {
    BASE_LOCATIONS
        .iter()
        .chain(NEVER_FORWARDED.iter())
        .copied()
        .collect()
}

fn received<'a>(
    environment: &'a std::collections::BTreeMap<String, String>,
    names: impl Iterator<Item = &'a str>,
) -> Vec<(String, String)> {
    names
        .filter_map(|name| {
            environment
                .get(name)
                .map(|value| (name.to_owned(), value.clone()))
        })
        .collect()
}

#[tokio::test]
async fn cursor_children_see_the_users_xdg_directories() {
    let environments = child_environments("cursor", &offered_everything()).await;
    assert_eq!(
        environments.len(),
        2,
        "expected a probe and a session child"
    );
    let expected: Vec<(String, String)> = BASE_LOCATIONS
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    for (launch, environment) in environments.iter().enumerate() {
        let names = BASE_LOCATIONS.iter().map(|(name, _)| *name);
        assert_eq!(
            received(environment, names),
            expected,
            "expected XDG_* to reach cursor launch #{launch} | received {environment:?}"
        );
    }
}

#[tokio::test]
async fn no_profile_child_receives_a_credential_proxy_or_session_directory() {
    for profile in mango_agent_acp::builtin_profiles() {
        let id = profile.id.to_string();
        for (launch, environment) in child_environments(&id, &offered_everything())
            .await
            .iter()
            .enumerate()
        {
            let names = NEVER_FORWARDED.iter().map(|(name, _)| *name);
            assert_eq!(
                received(environment, names),
                Vec::<(String, String)>::new(),
                "expected no credential, proxy or session directory for {id} launch #{launch} | received {environment:?}"
            );
        }
    }
}

#[tokio::test]
async fn each_profile_child_receives_the_location_its_vendor_documents() {
    for (profile, key, value) in PROFILE_LOCATIONS {
        let environments = child_environments(profile, &[(key, value)]).await;
        for (launch, environment) in environments.iter().enumerate() {
            assert_eq!(
                environment.get(key).map(String::as_str),
                Some(value),
                "expected {key}={value} to reach {profile} launch #{launch} | received {environment:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_profile_location_is_not_a_wildcard_for_the_other_profiles() {
    let offered: Vec<(&'static str, &'static str)> = PROFILE_LOCATIONS
        .iter()
        .map(|(_, key, value)| (*key, *value))
        .collect();
    for profile in mango_agent_acp::builtin_profiles() {
        let id = profile.id.to_string();
        let own = PROFILE_LOCATIONS
            .iter()
            .find(|(candidate, _, _)| *candidate == id)
            .map(|(_, key, _)| *key);
        for environment in child_environments(&id, &offered).await {
            let names = PROFILE_LOCATIONS.iter().map(|(_, key, _)| *key);
            let keys: Vec<String> = received(&environment, names)
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            let expected: Vec<String> = own.map(str::to_owned).into_iter().collect();
            assert_eq!(
                keys, expected,
                "expected only {id}'s own location variable | received {environment:?}"
            );
        }
    }
}
