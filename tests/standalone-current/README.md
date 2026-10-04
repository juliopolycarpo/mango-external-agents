# Independent Rust host

Copy this directory outside the repository. It has its own workspace and lockfile, and resolves
all four published SDK crates at `=0.4.2` through crates.io. This is the **current-release**
consumer: its pin follows the latest published release (bumped after each release, see
`docs/releasing.md`), and CI runs it on the declared minimum Rust version. It is evidence for the
published release it names, not for unpublished code in the repository. The fixed
`=0.3.1` control is `tests/standalone-historical`. No sibling checkout, HTTP service,
MangoStudio component, Mango Protocol, storage port or recovery record is needed.

```sh
cargo test --locked
cargo test --locked --all-features
cargo run --locked --features bundled-launcher -- codex
```

Rust 1.97 or newer and Tokio are required. The executable accepts `claude`, `codex` or `cursor`.
Install the chosen vendor CLI yourself and authenticate through its own commands. The SDK never
logs in. Running the CLI authorizes its current directory. A service must authorize the workspace
before constructing `HostContext` and must keep its own consent and approval policy.

`src/lib.rs` contains the short lifecycle. It accepts any `ProcessLauncher` through `HostContext`.
`src/main.rs` enables the optional `launcher-tokio` implementation and cancels on Ctrl+C. The
example refuses all tool approvals, reads the terminal, then closes. Event diagnostics print safe
metadata; a product can render bounded text through the typed event fields.

The named test installation drives ACP with the published `FakeAcpAgent`, Claude with copied
captures, and Codex with a named app-server fake. Each harness runs discovery and a real harness
session over injected byte-I/O. A second test cancels live work and checks that no fake child remains.
These are deterministic integration tests, not live vendor or native process qualification.

The two Claude files under `tests/fixtures` are byte-for-byte copies of repository captures:
`fixtures/claude/help/2.1.260.txt` and `fixtures/claude/transcripts/read-turn.jsonl`. Update them only
by copying a newly captured source; never edit vendor output by hand.

From the SDK root, `scripts/check-standalone.sh current` copies this entire directory into a temporary
workspace and verifies that every SDK package source is a registry entry before compiling it.
The advanced `examples/hub-host` orchestration is independent and optional.
