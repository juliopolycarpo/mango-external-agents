#!/usr/bin/env python3
"""Rejects a third-party version requirement written outside `[workspace.dependencies]`.

The root manifest declares every dependency once: the version, and whether the defaults are off.
A member takes it with `workspace = true` and lists the features it uses. Two rules follow.

A member dependency that carries its own `version` is refused, because a second copy of a
requirement is one that can ask for a different version. `agent-client-protocol-schema` is the one
exception: AGENTS.md keeps it inside mango-agent-acp, pinned exactly, so it must not be offered to
the other members.

A `[workspace.dependencies]` entry that lists `features` is refused, because features there are
added to every member that inherits the entry and no member can take them off again.

Usage: check-workspace-dependencies.py ROOT_MANIFEST MEMBER_MANIFEST...
"""
import sys
import tomllib

# Kept out of the workspace table on purpose; see the module documentation.
MEMBER_ONLY = {"agent-client-protocol-schema"}


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


def member_offenders(path):
    """Yields a line for each dependency of the member at `path` that names its own version."""
    for label, table in dependency_tables(load(path)):
        for name, spec in table.items():
            inherits = isinstance(spec, dict) and spec.get("workspace") is True
            if not inherits and name not in MEMBER_ONLY:
                yield f"{path}: [{label}] {name} = {spec}"


def workspace_offenders(path):
    """Yields a line for each `[workspace.dependencies]` entry of `path` that lists features."""
    entries = load(path).get("workspace", {}).get("dependencies", {})
    for name, spec in entries.items():
        if isinstance(spec, dict) and "features" in spec:
            yield f"{path}: [workspace.dependencies] {name} = {spec}"


def main(arguments):
    if len(arguments) < 2:
        print(
            "expected a root manifest and at least one member manifest, received "
            f"{len(arguments)} path(s)",
            file=sys.stderr,
        )
        return 2
    root, members = arguments[0], arguments[1:]
    own_version = [line for member in members for line in member_offenders(member)]
    with_features = list(workspace_offenders(root))
    if own_version:
        print(
            "expected each member dependency to use workspace = true, so its version is written "
            "once in [workspace.dependencies], received:",
            file=sys.stderr,
        )
        for line in own_version:
            print(f"  {line}", file=sys.stderr)
    if with_features:
        print(
            "expected each [workspace.dependencies] entry to carry no features, because a member "
            "cannot remove a feature it inherits, received:",
            file=sys.stderr,
        )
        for line in with_features:
            print(f"  {line}", file=sys.stderr)
    return 1 if own_version or with_features else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
