#!/usr/bin/env python3
"""Reject dependency declarations that bypass the workspace's version and feature policy.

Members inherit each dependency with ``workspace = true`` and choose their own additive features.
The workspace chooses versions and default features, and cannot impose features on every member.
The sole exception is the exact, feature-free ``agent-client-protocol-schema`` pin in the ACP
manifest. No other manifest or workspace entry may name that crate.

Usage: check-workspace-dependencies.py ROOT_MANIFEST MEMBER_MANIFEST...
"""
import re
import sys
import tomllib
from pathlib import Path

SCHEMA = "agent-client-protocol-schema"
EXACT_VERSION = re.compile(
    r"=[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?"
)


def load(path):
    with open(path, "rb") as handle:
        return tomllib.load(handle)


def dependency_tables(manifest):
    """Yields (label, table) for every dependency table of a member, target-specific ones too."""
    kinds = ("dependencies", "dev-dependencies", "build-dependencies")
    for kind in kinds:
        yield kind, manifest.get(kind, {})
    for target, body in manifest.get("target", {}).items():
        for kind in kinds:
            yield f"target.{target}.{kind}", body.get(kind, {})


def member_offenders(path, schema_manifest):
    """Yields a reason and declaration for each member dependency that violates the policy."""
    for label, table in dependency_tables(load(path)):
        for name, spec in table.items():
            line = f"{path}: [{label}] {name} = {spec}"
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if name == SCHEMA or package == SCHEMA:
                version = spec.get("version") if isinstance(spec, dict) else None
                valid_schema = (
                    Path(path).resolve() == schema_manifest
                    and label == "dependencies"
                    and name == SCHEMA
                    and package == SCHEMA
                    and isinstance(version, str)
                    and EXACT_VERSION.fullmatch(version) is not None
                    and spec.get("default-features") is False
                    and "features" not in spec
                    and "default_features" not in spec
                    and not spec.get("workspace")
                )
                if not valid_schema:
                    yield "schema", line
                continue
            inherits = isinstance(spec, dict) and spec.get("workspace") is True
            if not inherits:
                yield "inheritance", line
            elif "default-features" in spec or "default_features" in spec:
                yield "defaults", line


def workspace_offenders(path):
    """Yields a reason and declaration for each disallowed workspace dependency entry."""
    entries = load(path).get("workspace", {}).get("dependencies", {})
    for name, spec in entries.items():
        line = f"{path}: [workspace.dependencies] {name} = {spec}"
        package = spec.get("package", name) if isinstance(spec, dict) else name
        if name == SCHEMA or package == SCHEMA:
            yield "schema", line
        if isinstance(spec, dict) and "features" in spec:
            yield "features", line


def main(arguments):
    if len(arguments) < 2:
        print(
            "expected a root manifest and at least one member manifest, received "
            f"{len(arguments)} path(s)",
            file=sys.stderr,
        )
        return 2
    root, members = arguments[0], arguments[1:]
    schema_manifest = Path(root).resolve().parent / "crates/mango-agent-acp/Cargo.toml"
    found = list(workspace_offenders(root))
    found.extend(
        offender for member in members for offender in member_offenders(member, schema_manifest)
    )
    expectations = {
        "inheritance": "expected each member dependency to use workspace = true, received:",
        "defaults": "expected inherited dependencies to omit default-features so the workspace "
        "controls defaults, received:",
        "features": "expected workspace dependency entries to omit features, received:",
        "schema": f"expected {SCHEMA} only as an exact pin with default-features = false and "
        "no features in crates/mango-agent-acp/Cargo.toml [dependencies], received:",
    }
    for kind, expectation in expectations.items():
        lines = [line for found_kind, line in found if found_kind == kind]
        if lines:
            print(expectation, file=sys.stderr)
            for line in lines:
                print(f"  {line}", file=sys.stderr)
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
