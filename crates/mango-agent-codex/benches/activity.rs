//! Direct item completion costs; schema-shaped inputs and no vendor process.

mod support;

use mango_agent_codex::activity::completed;
use mango_agent_codex::protocol::items::ThreadItem;
use mango_external_agents::content::ActivityContent;
use mango_external_agents::event::ActivityStatus;
use serde_json::json;
use support::{Bench, Unit};

fn main() {
    let bench = Bench::new("codex activity completion");
    let changes: Vec<_> = (0..5)
        .map(|n| json!({"path": format!("change-{n}.rs"), "diff": "x".repeat(100_000)}))
        .collect();
    let cases = [
        (
            "completion/file-change-500KB",
            20,
            json!({
                "type": "fileChange", "id": "patch", "status": "completed", "changes": changes
            }),
        ),
        (
            "completion/command-1KiB",
            1000,
            json!({
                "type": "commandExecution", "id": "command", "status": "completed",
                "command": "cargo check --workspace", "cwd": "/workspace",
                "aggregatedOutput": "x".repeat(1024), "exitCode": 0
            }),
        ),
        (
            "completion/attributed-subagent",
            1000,
            json!({
                "type": "subAgentActivity", "id": "child-item", "kind": "started",
                "agentThreadId": "child-thread", "agentPath": "root/child"
            }),
        ),
        (
            "completion/suppressed-user-echo",
            1000,
            json!({
                "type": "userMessage", "id": "echo"
            }),
        ),
    ];
    for (name, count, raw) in cases {
        let item: ThreadItem = serde_json::from_value(raw).expect("valid schema-shaped item");
        let result = completed(&item);
        if name.ends_with("suppressed-user-echo") {
            assert!(result.is_none(), "a user-input echo must stay suppressed");
        } else {
            let result = result.expect("a rendered item must complete");
            assert_eq!(result.status, ActivityStatus::Completed);
            match (name, result.content) {
                ("completion/file-change-500KB", Some(ActivityContent::Diff { files })) => {
                    assert_eq!(files.len(), 5);
                    for (n, file) in files.iter().enumerate() {
                        assert_eq!(file.path, format!("change-{n}.rs"));
                        assert_eq!(
                            file.unified_diff.as_deref(),
                            Some("x".repeat(100_000).as_str())
                        );
                    }
                }
                ("completion/command-1KiB", Some(ActivityContent::Output { text })) => {
                    assert_eq!(text, "x".repeat(1024));
                    assert_eq!(result.detail, Some(format!("exit 0\n{}", "x".repeat(1024))));
                }
                ("completion/attributed-subagent", None) => {}
                (scenario, content) => panic!("unexpected content for {scenario}: {content:?}"),
            }
        }
        bench.run(
            name,
            Unit::new(count, "item"),
            || (),
            |()| {
                for _ in 0..count {
                    drop(std::hint::black_box(completed(std::hint::black_box(&item))));
                }
            },
        );
    }
}
