//! Ownership of a capture's scratch directory is separate from its output fixtures.

use std::path::PathBuf;

pub(crate) struct CaptureWorkspace {
    pub path: PathBuf,
    temporary: bool,
}

impl CaptureWorkspace {
    /// Creates a scratch directory or retains a caller-owned workspace. Example: `--workspace /tmp/capture`.
    pub fn new(explicit: Option<PathBuf>) -> Result<Self, String> {
        let temporary = explicit.is_none();
        let path = explicit.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("mea-capture-{}", crate::uuid_like()))
        });
        let created = if temporary {
            std::fs::create_dir(&path)
        } else {
            std::fs::create_dir_all(&path)
        };
        created.map_err(|error| {
            format!("expected a writable capture workspace {path:?}, received {error}")
        })?;
        Ok(Self { path, temporary })
    }
}

impl Drop for CaptureWorkspace {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_dir_all(&self.path);
        }
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
