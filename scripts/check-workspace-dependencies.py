#!/usr/bin/env python3
"""Reject dependency declarations that bypass the workspace's version and feature policy.

Members inherit each dependency with ``workspace = true`` and choose their own additive features.
The workspace chooses versions and default features; members cannot explicitly request ``default``.
Dependency feature forwarding in ``[features]`` follows the same rules. These checks govern
manifest entries, not Cargo's feature unification across dependency edges.
The sole exception is the exact, feature-free ``agent-client-protocol-schema`` registry pin in the
ACP manifest. No other manifest or workspace entry may name that crate, and root patches and
replacements cannot override it. Cargo configuration outside these manifests is not checked.

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


def member_offenders(path, schema_manifest, workspace_schema_names):
    """Yields a reason and declaration for each member entry that violates the policy."""
    manifest = load(path)
    schema_names = {SCHEMA} | workspace_schema_names
    for label, table in dependency_tables(manifest):
        for name, spec in table.items():
            line = f"{path}: [{label}] {name} = {spec}"
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if name == SCHEMA or package == SCHEMA:
                schema_names.add(name)
                version = spec.get("version") if isinstance(spec, dict) else None
                valid_schema = (
                    Path(path).resolve() == schema_manifest
                    and label == "dependencies"
                    and name == SCHEMA
                    and package == SCHEMA
                    and isinstance(version, str)
                    and EXACT_VERSION.fullmatch(version) is not None
                    and spec.get("default-features") is False
                    and set(spec) == {"version", "default-features"}
                )
                if not valid_schema:
                    yield "schema", line
                continue
            inherits = isinstance(spec, dict) and spec.get("workspace") is True
            if not inherits:
                yield "inheritance", line
            elif (
                "default-features" in spec
                or "default_features" in spec
                or (isinstance(spec.get("features"), list) and "default" in spec["features"])
            ):
                yield "defaults", line

    for feature_name, forwards in manifest.get("features", {}).items():
        if not isinstance(forwards, list):
            continue  # Cargo reports malformed feature tables.
        for forward in forwards:
            if not isinstance(forward, str) or "/" not in forward:
                continue
            dependency, feature = forward.split("/", 1)
            dependency = dependency.removesuffix("?")
            line = f"{path}: [features] {feature_name} = {forward}"
            if dependency in schema_names:
                yield "schema", line
            elif feature == "default":
                yield "defaults", line


def workspace_offenders(path):
    """Yields a reason and declaration for each disallowed workspace dependency entry."""
    manifest = load(path)
    entries = manifest.get("workspace", {}).get("dependencies", {})
    for name, spec in entries.items():
        line = f"{path}: [workspace.dependencies] {name} = {spec}"
        package = spec.get("package", name) if isinstance(spec, dict) else name
        if name == SCHEMA or package == SCHEMA:
            yield "schema", line
        if isinstance(spec, dict) and "features" in spec:
            yield "features", line

    for registry, patches in manifest.get("patch", {}).items():
        for name, spec in patches.items():
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if name == SCHEMA or package == SCHEMA:
                yield "schema", f"{path}: [patch.{registry}] {name} = {spec}"

    for package_id, spec in manifest.get("replace", {}).items():
        source, separator, fragment = package_id.partition("#")
        if separator and fragment[:1].isdigit():
            # A version-only fragment takes its package name from the URL's last segment.
            name = source.rstrip("/").rsplit("/", 1)[-1]
        else:
            name = fragment if separator else source
            name = name.split("@", 1)[0].split(":", 1)[0]
        if name == SCHEMA:
            yield "schema", f"{path}: [replace] {package_id} = {spec}"


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
    workspace = load(root).get("workspace", {}).get("dependencies", {})
    workspace_schema_names = {
        name for name, spec in workspace.items()
        if name == SCHEMA or (isinstance(spec, dict) and spec.get("package") == SCHEMA)
    }
    found = list(workspace_offenders(root))
    found.extend(
        offender
        for member in members
        for offender in member_offenders(member, schema_manifest, workspace_schema_names)
    )
    expectations = {
        "inheritance": "expected each member dependency to use workspace = true, received:",
        "defaults": "expected inherited dependencies to omit default-features and the default "
        "feature so the workspace controls defaults, received:",
        "features": "expected workspace dependency entries to omit features, received:",
        "schema": f"expected {SCHEMA} only as an exact pin with default-features = false and "
        "no other keys in crates/mango-agent-acp/Cargo.toml [dependencies] and no forwarded "
        "schema features, root patches or replacements, received:",
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
