#!/usr/bin/env python3
"""Rejects a published crate's dev-dependency that names only a path.

`cargo package` drops such an entry from the packaged manifest, so the features it turned on are
gone when the tarball's tests are built. `workspace = true`, or a `version` beside the `path`,
keeps it. The manifest is parsed as TOML, so dotted keys, inline tables and comments are all read
the way Cargo reads them.

Usage: check-dev-dependencies.py MANIFEST...
"""
import sys
import tomllib


def dev_dependency_tables(manifest):
    """Yields (label, table) for `[dev-dependencies]` and every `[target.*.dev-dependencies]`."""
    yield "dev-dependencies", manifest.get("dev-dependencies", {})
    for target, body in manifest.get("target", {}).items():
        yield f"target.{target}.dev-dependencies", body.get("dev-dependencies", {})


def offenders(path):
    with open(path, "rb") as handle:
        manifest = tomllib.load(handle)
    for label, table in dev_dependency_tables(manifest):
        for name, spec in table.items():
            if not isinstance(spec, dict):
                continue
            if "path" in spec and "version" not in spec and not spec.get("workspace"):
                yield f'{path}: [{label}] {name} = {spec}'


def main(paths):
    if not paths:
        print("expected at least one Cargo.toml path, received none", file=sys.stderr)
        return 2
    found = [line for path in paths for line in offenders(path)]
    if found:
        print(
            "expected each dev-dependency to use workspace = true or carry a version beside its "
            "path, because cargo drops a path-only entry from the published manifest, received:",
            file=sys.stderr,
        )
        for line in found:
            print(f"  {line}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
