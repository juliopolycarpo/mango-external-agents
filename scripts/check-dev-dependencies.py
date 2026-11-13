#!/usr/bin/env python3
"""Rejects a published crate's dev-dependency that packaging drops or that adds nothing.

`cargo package` drops an entry that names only a path from the packaged manifest, so the features
it turned on are gone when the tarball's tests are built. `workspace = true`, or a `version` beside
the `path`, keeps it.

A dev-dependency whose specification equals the `[dependencies]` entry of the same name adds
nothing: tests already see every normal dependency. It is refused so a second copy of a version
requirement cannot drift from the first. An entry that turns on a feature the normal one does not
is a different specification and stays. Target dev entries compare with root normal dependencies
and the same target's normal table, never another target's table.

The manifest is parsed as TOML, so dotted keys, inline tables and comments are all read the way
Cargo reads them.

Usage: check-dev-dependencies.py MANIFEST...
"""
import sys
import tomllib


def dev_dependency_tables(manifest):
    """Yields each dev table with the normal tables applicable to that exact target."""
    dependencies = manifest.get("dependencies", {})
    yield "dev-dependencies", manifest.get("dev-dependencies", {}), (dependencies,)
    for target, body in manifest.get("target", {}).items():
        yield (
            f"target.{target}.dev-dependencies",
            body.get("dev-dependencies", {}),
            (dependencies, body.get("dependencies", {})),
        )


def specification(spec):
    """A dependency as a table, so `"1.0"` and `{ version = "1.0" }` compare equal."""
    return spec if isinstance(spec, dict) else {"version": spec}


def offenders(path):
    """Yields (kind, line) for each dev-dependency of the manifest at `path` that is refused."""
    with open(path, "rb") as handle:
        manifest = tomllib.load(handle)
    for label, table, normal_tables in dev_dependency_tables(manifest):
        for name, spec in table.items():
            line = f'{path}: [{label}] {name} = {spec}'
            # Compare both candidates: a different target entry must not hide a root match.
            if any(
                name in dependencies and specification(spec) == specification(dependencies[name])
                for dependencies in normal_tables
            ):
                yield "redundant", line
            if not isinstance(spec, dict):
                continue
            if "path" in spec and "version" not in spec and not spec.get("workspace"):
                yield "dropped", line


def main(paths):
    if not paths:
        print("expected at least one Cargo.toml path, received none", file=sys.stderr)
        return 2
    found = [offender for path in paths for offender in offenders(path)]
    expectations = {
        "dropped": "expected each dev-dependency to use workspace = true or carry a version beside "
        "its path, because cargo drops a path-only entry from the published manifest, received:",
        "redundant": "expected each dev-dependency to differ from the [dependencies] entry of the "
        "same name, because tests already see every normal dependency, received:",
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
