#!/usr/bin/env python3
"""Exercise the scheduled publisher's actual shell block with untrusted result files."""

import os
from pathlib import Path
import subprocess
import tempfile
import textwrap

workflow = Path(".github/workflows/vendor-drift.yml").read_text()
assert "  latest-capture:\n" in workflow, "expected vendor capture in a separate read-only job"
capture, reporter = workflow.split("  latest-capture:\n", 1)[1].split("  latest-report:\n", 1)
assert "issues: write" not in capture, "expected no issue-write permission in the vendor runner"
assert "scripts/upsert-vendor-drift-issue.sh" not in capture, "expected issue updates on a clean runner"
assert "needs: latest-capture" in reporter, "expected capture results before publication"
assert "ref: ${{ github.sha }}" in reporter, "expected a checkout of the trusted scheduled commit"
assert "$GITHUB_PATH" not in reporter, "expected no artifact directory on the publisher PATH"
assert "scripts/install-vendor-cli.sh" not in reporter, "expected no vendor execution in the publisher"

step = reporter.split("      - name: Validate and publish the drift result\n", 1)[1]
block = step.split("        run: |\n", 1)[1]
lines = []
for line in block.splitlines():
    if line and not line.startswith("          "):
        break
    lines.append(line)
command = textwrap.dedent("\n".join(lines)).replace("${{ matrix.vendor }}", "claude")

# A named fake owns the external issue write. Everything before it is the real workflow code.
with tempfile.TemporaryDirectory(prefix="mea-result-tests-") as temporary:
    root = Path(temporary)
    scripts = root / "scripts"
    scripts.mkdir()
    fake = scripts / "upsert-vendor-drift-issue.sh"
    fake.write_text('#!/bin/sh\nprintf "called\\n" >> "$FAKE_ISSUE_LOG"\n')
    fake.chmod(0o755)
    marker = "<!-- vendor-drift:claude -->\n"
    cases = [
        ("no-drift", "no-drift\n", None, None, 0, False),
        ("valid", "drift\n", marker + "public diff\n", None, 0, True),
        ("missing-report", "drift\n", None, None, 2, False),
        ("wrong-marker", "drift\n", "<!-- vendor-drift:codex -->\n", None, 2, False),
        ("oversized-report", "drift\n", marker + "x" * 65536, None, 2, False),
        ("oversized-state", "x" * 17, marker, None, 2, False),
        ("invalid-state", "unknown\n", marker, None, 2, False),
        ("failed-capture", "error\n", None, None, 2, False),
        ("symlink-state", "drift\n", marker, "state", 2, False),
        ("symlink-report", "drift\n", marker, "report.md", 2, False),
    ]
    for name, state, report, symlink, expected_status, expected_write in cases:
        runner_temp = root / name
        result = runner_temp / "vendor-drift-result"
        result.mkdir(parents=True)
        (result / "state").write_text(state)
        if report is not None:
            (result / "report.md").write_text(report)
        if symlink:
            original = result / symlink
            target = result / "linked-data"
            original.rename(target)
            original.symlink_to(target)
        log = runner_temp / "issue-writes"
        environment = dict(os.environ, RUNNER_TEMP=str(runner_temp),
                           GITHUB_REPOSITORY="owner/repository", GH_TOKEN="fake-test-token",
                           FAKE_ISSUE_LOG=str(log))
        completed = subprocess.run(["bash", "-euo", "pipefail", "-c", command],
                                   cwd=root, env=environment, capture_output=True, text=True)
        assert completed.returncode == expected_status, (
            f"expected {name} status {expected_status}, received {completed.returncode}: {completed.stderr}"
        )
        assert log.exists() == expected_write, f"unexpected issue write for {name}"

print("vendor result isolation and artifact tests passed")
