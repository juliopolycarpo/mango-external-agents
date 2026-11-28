#!/usr/bin/env python3
"""Run scripts/check-tool-pins.py against throwaway workflow directories and the real one."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
CHECK = ROOT / "scripts/check-tool-pins.py"


def workflow(*tool_lines):
    """One workflow with one job; each argument is the `tool:` value of an install-action step."""
    steps = "".join(
        "      - uses: taiki-e/install-action@fe3acb53a98b62c772db35de75ce0e4595e7ca8c # v2.90.8\n"
        f"        with:\n          tool: {tools}\n"
        for tools in tool_lines
    )
    return f"on: push\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n{steps}"


class WorkflowDirectory:
    """A temporary `.github/workflows` stand-in holding the named workflow files."""

    def __init__(self, **files):
        self.temporary = tempfile.TemporaryDirectory(prefix="mea-tool-pins-test.")
        self.path = Path(self.temporary.name)
        for name, content in files.items():
            (self.path / f"{name}.yml").write_text(content, encoding="utf-8")

    def run(self, *arguments):
        return subprocess.run(
            [sys.executable, "-I", str(CHECK), *arguments, str(self.path)],
            capture_output=True,
            text=True,
            check=False,
        )


class ToolPins(unittest.TestCase):
    def test_exact_versions_pass_and_are_counted(self):
        directory = WorkflowDirectory(
            ci=workflow("cargo-nextest@0.9.152", "cargo-deny@0.22.0,dprint@0.65.0"),
            release=workflow("cargo-nextest@0.9.152"),
        )
        result = directory.run()
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertEqual(result.stdout, "tool pins: 3 tools, each at one exact version\n")

    def test_list_prints_one_line_per_tool(self):
        directory = WorkflowDirectory(ci=workflow("dprint@0.65.0, cargo-deny@0.22.0"))
        result = directory.run("--list")
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertEqual(result.stdout, "cargo-deny 0.22.0\ndprint 0.65.0\n")

    def test_an_unversioned_tool_is_refused_naming_the_job_and_the_entry(self):
        directory = WorkflowDirectory(ci=workflow("cargo-deny@0.22.0,dprint"))
        result = directory.run()
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            "ci.yml:lint: expected an install-action tool as name@x.y.z | received: dprint\n",
        )

    def test_a_partial_or_floating_version_is_refused(self):
        for entry in ("cargo-hack@0.6", "cargo-hack@latest", "cargo-hack@^0.6.46", "cargo-hack@"):
            with self.subTest(entry=entry):
                result = WorkflowDirectory(ci=workflow(entry)).run()
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"expected an install-action tool as name@x.y.z | received: {entry}", result.stderr)

    def test_an_install_step_without_a_tool_is_refused(self):
        content = workflow("cargo-deny@0.22.0").replace("        with:\n          tool: cargo-deny@0.22.0\n", "")
        result = WorkflowDirectory(ci=content).run()
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            "ci.yml:lint: expected an install-action tool as name@x.y.z | received: no tool\n",
        )

    def test_two_versions_of_one_tool_are_refused_naming_both_places(self):
        directory = WorkflowDirectory(
            ci=workflow("cargo-nextest@0.9.152"),
            release=workflow("cargo-nextest@0.9.151"),
        )
        result = directory.run()
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            result.stderr,
            "cargo-nextest: expected one version in every workflow | received: "
            "0.9.151 in release.yml:lint; 0.9.152 in ci.yml:lint\n",
        )

    def test_a_directory_that_installs_nothing_is_refused(self):
        directory = WorkflowDirectory(ci="on: push\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n")
        result = directory.run()
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected at least one install-action tool under", result.stderr)
        self.assertIn("| received none", result.stderr)

    def test_an_unknown_flag_is_a_usage_error(self):
        result = WorkflowDirectory(ci=workflow("dprint@0.65.0")).run("--fix")
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage: check-tool-pins.py [--list] [WORKFLOW_DIRECTORY] | received: --fix", result.stderr)

    def test_the_repository_workflows_pass(self):
        result = subprocess.run(
            [sys.executable, "-I", str(CHECK), str(ROOT / ".github/workflows")],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual((result.returncode, result.stderr), (0, ""))


if __name__ == "__main__":
    unittest.main()
