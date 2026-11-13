#![doc = include_str!("../README.md")]

pub mod argv;
pub mod auth;
pub mod cli_surface;
pub mod commands;
pub mod deadlines;
pub mod harness;
pub mod help;
pub mod mcp;
pub mod models;
pub mod permissions;
pub mod pinned;
pub mod probe;
pub mod protocol;
mod redacted;
pub mod reducer;
pub mod session;
pub mod version;

pub use harness::{ClaudeHarness, harness};
pub use session::ClaudeSession;

/// The id this crate's harness registers under.
///
/// A function rather than a `&str` constant, so a caller holds the type the registry keys on
/// instead of a string it has to re-validate — and so a caller cannot look one up with a value no
/// harness could have registered.
///
/// # Example
///
/// ```
/// assert_eq!(mango_agent_claude::harness_id().as_str(), "claude");
/// ```
pub fn harness_id() -> mango_external_agents::HarnessId {
    mango_external_agents::HarnessId::claude()
}

/// The fake `claude` launcher the integration tests share, compiled once for the unit tests that
/// need a launcher too.
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

#[cfg(test)]
mod tests {
    use super::harness_id;

    #[test]
    fn the_harness_registers_under_the_id_this_crate_publishes() {
        let expected = env!("CARGO_PKG_NAME").trim_start_matches("mango-agent-");
        assert_eq!(harness_id().as_str(), expected);
        assert_eq!(
            super::harness().descriptor().id(),
            &harness_id(),
            "expected the published id to be the one the descriptor registers under"
        );
    }

    #[test]
    fn scratch_allocation_skips_existing_directories_and_files() {
        use std::sync::atomic::AtomicU64;

        let parent = crate::test_support::private_scratch("collision-parent");
        let candidate =
            |index| parent.join(format!("mea-collision-{}-{index}", std::process::id()));
        let directory = candidate(0);
        let file = candidate(1);
        crate::test_support::private_directory(&directory);
        std::fs::write(&file, b"retained fixture").expect("expected to create the collision file");
        let next = AtomicU64::new(0);
        let allocated = std::panic::catch_unwind(|| {
            crate::test_support::private_scratch_with_counter(&parent, "collision", &next)
        });
        assert!(
            allocated.is_ok(),
            "expected allocation to skip existing candidates {} and {} | received a panic",
            directory.display(),
            file.display()
        );
        let allocated = allocated.expect("expected a fresh scratch directory");
        assert_eq!(allocated, candidate(2));
        assert!(
            directory.is_dir(),
            "expected the pre-existing directory to remain"
        );
        assert_eq!(
            std::fs::read(&file).expect("expected the collision file"),
            b"retained fixture"
        );
        std::fs::remove_dir_all(parent).expect("expected to remove the test's private parent");
    }

    /// The shared scratch helper, checked where it is compiled once. Under `umask 002` a plain
    /// `create_dir` made a group-writable root, which the harness refuses for an MCP artifact.
    #[test]
    fn a_test_scratch_root_is_private_whatever_the_umask_and_never_shared() {
        let first = crate::test_support::private_scratch("scratch-helper");
        let second = crate::test_support::private_scratch("scratch-helper");
        assert_ne!(
            first, second,
            "expected two calls to name two directories | received the same path twice"
        );
        #[cfg(unix)]
        for root in [&first, &second] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root)
                .expect("expected the scratch root to exist")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "expected an owner-only scratch root (0o700) | received {mode:#o}"
            );
        }
        for root in [first, second] {
            let child = root.join("named-private-directory");
            crate::test_support::private_directory(&child);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&child)
                    .expect("expected the named private directory")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(
                    mode,
                    0o700,
                    "expected named private directory {} mode 0o700 | received {mode:#o}",
                    child.display()
                );
            }
            let artifact = child.join("artifact");
            std::fs::write(&artifact, b"writable")
                .expect("expected the private directory to allow owner writes");
            std::fs::remove_file(artifact).expect("expected to remove the test artifact");
            std::fs::remove_dir(child).expect("expected to remove the named private directory");
            std::fs::remove_dir(root).expect("expected to remove the empty scratch root");
        }
    }

    #[cfg(unix)]
    #[test]
    fn scratch_helpers_restore_owner_permissions_under_a_restrictive_umask() {
        // Change only the child process's umask; tests in this process run concurrently.
        let output = std::process::Command::new("sh")
            .args(["-c", "umask 0277; exec \"$@\"", "scratch-umask-test"])
            .arg(std::env::current_exe().expect("expected this test executable"))
            .args([
                "--exact",
                "tests::a_test_scratch_root_is_private_whatever_the_umask_and_never_shared",
                "--nocapture",
            ])
            .output()
            .expect("expected to run the scratch helper test under umask 0277");
        assert!(
            output.status.success(),
            "expected writable owner-only scratch roots under umask 0277 | received {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
