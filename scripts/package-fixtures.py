#!/usr/bin/env python3
"""Synchronize crate-local test fixtures with canonical captures."""
import argparse
from pathlib import Path
import shutil
import sys

ROOT = Path(__file__).resolve().parent.parent
SELECTIONS = {
    "claude": [
        "help/2.1.227.txt", "help/2.1.260.txt", "help/2.1.270.txt",
        "transcripts/*.jsonl",
        "contract", "historical/contract",
    ],
    "codex": ["**/*.jsonl"],
    "acp": ["cursor/contract", "grok/contract", "opencode/contract"],
}


def selected_files(source, selections):
    """Return selected relative files, rejecting absent selections."""
    files = set()
    for selection in selections:
        matches = list(source.glob(selection))
        if not matches:
            raise ValueError(f"{source / selection}: expected fixture selection, received no files")
        for match in matches:
            if match.is_dir():
                files.update(path.relative_to(source) for path in match.rglob("*") if path.is_file())
            elif match.is_file():
                files.add(match.relative_to(source))
    return files


def synchronize(root, check):
    """Compare or replace derived files; return all differences."""
    differences = []
    for vendor, selections in SELECTIONS.items():
        source = root / "fixtures" / vendor
        destination = root / "crates" / f"mango-agent-{vendor}" / "fixtures" / vendor
        expected = selected_files(source, selections)
        actual = {path.relative_to(destination) for path in destination.rglob("*") if path.is_file()}
        for relative in sorted(expected | actual):
            original, copy = source / relative, destination / relative
            if relative not in expected:
                differences.append(f"{copy}: expected no extra fixture, received file")
            elif relative not in actual:
                differences.append(f"{copy}: expected fixture copy, received missing file")
            elif original.read_bytes() != copy.read_bytes():
                differences.append(f"{copy}: expected bytes from {original}, received different bytes")
        if not check:
            if destination.exists():
                shutil.rmtree(destination)
            for relative in sorted(expected):
                copy = destination / relative
                copy.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source / relative, copy)
    return differences


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="reject missing, stale, or extra copies")
    args = parser.parse_args()
    try:
        differences = synchronize(ROOT, args.check)
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    if args.check and differences:
        print("\n".join(differences), file=sys.stderr)
        return 1
    print("package fixture copies match canonical captures" if args.check else "package fixture copies regenerated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
