//! Ownership of a capture's scratch directory is separate from its output fixtures.

use std::path::PathBuf;

pub(crate) struct CaptureWorkspace {
    pub path: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl CaptureWorkspace {
    /// Creates a scratch directory or retains a caller-owned workspace. Example: `--workspace /tmp/capture`.
    pub fn new(explicit: Option<PathBuf>) -> Result<Self, String> {
        if let Some(path) = explicit {
            // Absolute before the directory exists, so a relative `--workspace` is the same place
            // for the agent's cwd as for this process, and nothing is created and then refused.
            let path = std::path::absolute(&path).map_err(|error| {
                format!("expected a capture workspace {path:?} resolvable to an absolute path, received {error}")
            })?;
            std::fs::create_dir_all(&path).map_err(|error| {
                format!("expected a writable capture workspace {path:?}, received {error}")
            })?;
            return Ok(Self {
                path,
                _temporary: None,
            });
        }
        let root = std::env::temp_dir();
        let directory = tempfile::Builder::new()
            .prefix("mea-capture-")
            .rand_bytes(12)
            .tempdir_in(&root)
            .map_err(|error| {
                format!("expected a unique capture workspace under {root:?}, received {error}")
            })?;
        Ok(Self {
            path: directory.path().to_owned(),
            _temporary: Some(directory),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_workspace_is_removed_when_capture_scope_ends() {
        let workspace = CaptureWorkspace::new(None).expect("scratch workspace");
        let path = workspace.path.clone();
        std::fs::write(path.join("generated-schema.json"), "{}").expect("generated file");
        drop(workspace);
        assert!(
            !path.exists(),
            "expected the implicit capture workspace to be removed after drop"
        );
    }

    #[test]
    fn a_relative_explicit_workspace_is_resolved_against_the_current_directory() {
        // The path is made absolute here, once, so that every capture sees the same directory and
        // `session/new`, which needs an absolute cwd, is never refused after the directory exists.
        let name = format!("mea-relative-workspace-{}", std::process::id());
        let expected = std::env::current_dir()
            .expect("current directory")
            .join(&name);

        let workspace = CaptureWorkspace::new(Some(PathBuf::from(&name))).expect("workspace");
        let created = expected.is_dir();
        let received = workspace.path.clone();
        std::fs::remove_dir_all(&expected)
            .expect("expected the relative workspace to be cleaned up");

        assert!(
            received.is_absolute(),
            "expected an absolute capture workspace for the relative {name:?} | received {received:?}"
        );
        assert_eq!(received, expected);
        assert!(
            created,
            "expected the workspace directory to be created at {expected:?}"
        );
    }

    #[test]
    fn explicit_workspace_remains_owned_by_the_caller() {
        let caller = CaptureWorkspace::new(None).expect("caller directory");
        let path = caller.path.clone();
        let capture = CaptureWorkspace::new(Some(path.clone())).expect("explicit workspace");
        drop(capture);
        assert!(
            path.is_dir(),
            "expected an explicit capture workspace to remain"
        );
    }
}
