//! `mea`: the unpublished smoke and capture CLI for mango-external-agents.
//!
//! Subcommands (`discover`, `turn`, `capture`, `doctor`) land in plan 006. Today it prints the
//! harness kinds linked into the binary, which proves the workspace wiring end to end.

/// Every harness kind the binary links, in registry order.
fn harness_kinds() -> [&'static str; 3] {
    [
        mango_agent_claude::HARNESS_KIND,
        mango_agent_codex::HARNESS_KIND,
        mango_agent_acp::HARNESS_KIND,
    ]
}

fn main() {
    println!(
        "mea {} (mango-external-agents {})",
        env!("CARGO_PKG_VERSION"),
        mango_external_agents::VERSION
    );
    for kind in harness_kinds() {
        println!("harness: {kind}");
    }
}

#[cfg(test)]
mod tests {
    use super::harness_kinds;

    #[test]
    fn harness_kinds_are_distinct() {
        let kinds = harness_kinds();
        let mut sorted = kinds.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            kinds.len(),
            "expected 3 distinct kinds, received {kinds:?}"
        );
    }
}
