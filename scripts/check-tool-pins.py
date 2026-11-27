#!/usr/bin/env python3
"""Require every tool a workflow installs through taiki-e/install-action to name an exact version.

An entry without a version installs the newest release the action knows, so a tool release can
change a verdict (a new cargo-deny default, a new clippy-adjacent lint in cargo-hack's output, a
reflow in dprint) on a commit that touched nothing. Each `tool:` entry has to be `name@x.y.z`, and
a tool has to carry the same version in every workflow, so the gate, the fresh-dependency lane and
the release verify job judge with the same binaries.

Dependabot does not read `tool:` strings: the versions move by hand, in every workflow at once.
`--list` prints `name version` per tool, for installing the set CI runs (`cargo install --locked
name@version`).

Usage: check-tool-pins.py [--list] [WORKFLOW_DIRECTORY]   # default: .github/workflows
"""
import pathlib
import re
import sys

try:
    import yaml
except ImportError as error:
    sys.exit(
        "(check-tool-pins): expected PyYAML importable by python3 (apt: python3-yaml) | "
        f"received {error}"
    )

INSTALL_ACTION = "taiki-e/install-action@"
EXACT = re.compile(r"^([A-Za-z0-9_-]+)@([0-9]+\.[0-9]+\.[0-9]+)$")


def installed_tools(directory):
    """Yields (where, entry) for every comma-separated `tool:` entry of an install-action step."""
    for workflow in sorted(pathlib.Path(directory).glob("*.yml")):
        document = yaml.safe_load(workflow.read_text(encoding="utf-8")) or {}
        for job_id, job in (document.get("jobs") or {}).items():
            for step in job.get("steps") or []:
                if not str(step.get("uses", "")).startswith(INSTALL_ACTION):
                    continue
                tools = str((step.get("with") or {}).get("tool", ""))
                where = f"{workflow.name}:{job_id}"
                if not tools.strip():
                    yield where, ""
                for entry in filter(None, (part.strip() for part in tools.split(","))):
                    yield where, entry


def check(directory):
    """Returns (problems, versions): the refusals, and {tool: {version: [where, ...]}}."""
    problems = []
    versions = {}
    for where, entry in installed_tools(directory):
        match = EXACT.match(entry)
        if not match:
            problems.append(
                f"{where}: expected an install-action tool as name@x.y.z | received: {entry or 'no tool'}"
            )
            continue
        versions.setdefault(match.group(1), {}).setdefault(match.group(2), []).append(where)
    for tool, found in sorted(versions.items()):
        if len(found) > 1:
            listed = "; ".join(f"{version} in {', '.join(sorted(set(places)))}" for version, places in sorted(found.items()))
            problems.append(f"{tool}: expected one version in every workflow | received: {listed}")
    if not versions and not problems:
        problems.append(f"expected at least one install-action tool under {directory} | received none")
    return problems, versions


def main(arguments):
    listing = "--list" in arguments
    positional = [argument for argument in arguments if argument != "--list"]
    if len(positional) > 1 or any(argument.startswith("-") for argument in positional):
        print(f"usage: check-tool-pins.py [--list] [WORKFLOW_DIRECTORY] | received: {' '.join(arguments)}", file=sys.stderr)
        return 2
    directory = positional[0] if positional else ".github/workflows"
    problems, versions = check(directory)
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    if listing:
        for tool, found in sorted(versions.items()):
            print(f"{tool} {next(iter(found))}")
    else:
        print(f"tool pins: {len(versions)} tools, each at one exact version")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
