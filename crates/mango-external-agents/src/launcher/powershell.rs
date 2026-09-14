//! Windows script entrypoints use PowerShell's literal file invocation, never command text.

use std::path::{Path, PathBuf};

use crate::process::LaunchSpec;

trait ScriptFiles {
    fn is_file(&self, path: &Path) -> bool;
}

struct LocalScriptFiles;

impl ScriptFiles for LocalScriptFiles {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// Builds a fallback launch for an installed `.ps1` CLI using only the supplied environment.
pub(super) fn fallback(spec: &LaunchSpec) -> Option<LaunchSpec> {
    script_launch(spec, &LocalScriptFiles)
}

fn script_launch(spec: &LaunchSpec, files: &impl ScriptFiles) -> Option<LaunchSpec> {
    let program = Path::new(spec.program()?);
    let extension = program.extension().and_then(|ext| ext.to_str());
    if extension.is_some_and(|ext| !ext.eq_ignore_ascii_case("ps1")) {
        return None;
    }
    let script_name = program.with_extension("ps1");
    let script = if program.is_absolute() || program.components().count() > 1 {
        let path = spec.cwd.join(script_name);
        files.is_file(&path).then_some(path)?
    } else {
        let path = environment(spec, "PATH")?;
        std::env::split_paths(path)
            // An empty element — a trailing or doubled `;`, which Windows `PATH` values carry
            // routinely — joins to the working directory, and that directory is the workspace the
            // agent was pointed at rather than a place the host installed a CLI. A `cursor-agent.ps1`
            // committed in a repository must never be what the launcher runs.
            .filter(|directory| !directory.as_os_str().is_empty())
            .map(|directory| spec.cwd.join(directory).join(&script_name))
            .find(|path| files.is_file(path))?
    };
    let root = PathBuf::from(environment(spec, "SystemRoot")?);
    let interpreter = root.join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut launch = spec.clone();
    launch.argv = vec![
        interpreter.to_str()?.to_owned(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-File".into(),
        script.to_str()?.to_owned(),
    ];
    launch.argv.extend_from_slice(&spec.argv[1..]);
    Some(launch)
}

fn environment<'a>(spec: &'a LaunchSpec, key: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    struct FakeScriptFiles(BTreeSet<PathBuf>);
    impl ScriptFiles for FakeScriptFiles {
        fn is_file(&self, path: &Path) -> bool {
            self.0.contains(path)
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            argv: vec!["cursor-agent".into(), "literal $(Get-Date); & text".into()],
            cwd: PathBuf::from(r"C:\workspace"),
            env: BTreeMap::from([
                ("Path".into(), r"C:\tools".into()),
                ("SYSTEMROOT".into(), r"C:\Windows".into()),
            ]),
            stdin: true,
            hide_window: true,
        }
    }

    #[test]
    fn script_arguments_and_host_policy_are_preserved() {
        let original = spec();
        let files = FakeScriptFiles(BTreeSet::from([PathBuf::from(
            r"C:\tools\cursor-agent.ps1",
        )]));
        let resolved = script_launch(&original, &files).expect("script on host PATH");
        assert_eq!(resolved.argv[4], "-File");
        assert_eq!(resolved.argv[6], original.argv[1]);
        assert_eq!(resolved.cwd, original.cwd);
        assert_eq!(resolved.env, original.env);
        assert!(resolved.stdin && resolved.hide_window);
        assert_eq!(environment(&original, "path"), Some(r"C:\tools"));
        assert_eq!(environment(&original, "LOCALAPPDATA"), None);
    }

    #[test]
    fn an_empty_path_element_does_not_reach_the_working_directory() {
        let mut original = spec();
        original.env.insert("Path".into(), r"C:\tools;".into());
        let files = FakeScriptFiles(BTreeSet::from([PathBuf::from(
            r"C:\workspace\cursor-agent.ps1",
        )]));

        assert!(
            script_launch(&original, &files).is_none(),
            "expected an empty PATH element not to resolve to the authorised workspace"
        );
    }

    #[test]
    fn absent_scripts_or_interpreter_configuration_do_not_use_ambient_paths() {
        let mut original = spec();
        let files = FakeScriptFiles(BTreeSet::new());
        assert!(script_launch(&original, &files).is_none());
        original.argv[0] = "cursor-agent.exe".into();
        assert!(script_launch(&original, &files).is_none());
        original.argv[0] = r"C:\tools\cursor-agent.ps1".into();
        original.env.clear();
        let files = FakeScriptFiles(BTreeSet::from([PathBuf::from(&original.argv[0])]));
        assert!(script_launch(&original, &files).is_none());
        assert!(fallback(&original).is_none());
    }
}
