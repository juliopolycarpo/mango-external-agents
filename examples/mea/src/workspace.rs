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
