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

/// Variables no child may receive: a session directory, credentials, and proxies that can carry
/// userinfo.
const NEVER_FORWARDED: [(&str, &str); 9] = [
    ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ("CURSOR_API_KEY", "canary-cursor-api-key"),
    ("CURSOR_AUTH_TOKEN", "canary-cursor-token"),
    ("XAI_API_KEY", "canary-xai-key"),
    ("OPENAI_API_KEY", "canary-openai-key"),
    ("GITHUB_TOKEN", "canary-github-token"),
    ("GEMINI_API_KEY", "canary-gemini-key"),
    ("HTTPS_PROXY", "http://user:canary-pass@proxy.example:3128"),
    ("ALL_PROXY", "socks5://user:canary-pass@proxy.example:1080"),
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
