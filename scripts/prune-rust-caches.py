#!/usr/bin/env python3
"""Prune superseded Swatinem/rust-cache generations from the Actions cache of refs/heads/main.

Every toolchain, .cargo/config.toml or Cargo.lock change saves a new cache generation under a new
key, and nothing restores the old ones again. rust-cache keys look like

    v0-rust-<name>-<os>-<arch>-<environment hash>-<lockfile hash>

where <name> is `shared-key`, or `key` plus the job id, and os and arch come from Node's
os.type() and os.arch(). A family is the key without its two trailing hashes. On refs/heads/main
the entry of each family with the newest created_at is kept and every older one is selected.

Never selected: an entry on another ref (an open pull request's cache), a key outside the
rust-cache grammar, and any entry tied with the newest of its family (the order is unknown).

The default is a dry run. Pass --delete to remove the selected entries.
Usage: scripts/prune-rust-caches.py --repo OWNER/REPO [--delete]
"""

import argparse
import json
import re
import subprocess
import sys
from datetime import datetime, timezone

MAIN_REF = "refs/heads/main"

# The whole key must match: the name is greedy and the OS token is fixed, so names with dashes
# (`msrv-windows`) and OS names with an underscore (`Windows_NT`) resolve without guessing.
KEY_GRAMMAR = re.compile(
    r"^(?P<family>v0-rust-[A-Za-z0-9_.-]+-(?:Linux|Darwin|Windows_NT)-[a-z0-9_]+)"
    r"-[0-9a-f]{8}-[0-9a-f]{8}$"
)

EXPECTED_LISTING = (
    '{"actions_caches": [{"id": int, "key": str, "ref": str, '
    '"size_in_bytes": int, "created_at": ISO 8601 str}]}'
)


class CacheListingError(Exception):
    """The listing GitHub returned is not the shape the selection needs."""


def parse_pages(text):
    """Read the JSON documents `gh api --paginate` concatenates, one per page."""
    decoder = json.JSONDecoder()
    pages = []
    position = 0
    while True:
        while position < len(text) and text[position].isspace():
            position += 1
        if position >= len(text):
            return pages
        try:
            page, position = decoder.raw_decode(text, position)
        except json.JSONDecodeError as error:
            raise CacheListingError(
                f"expected {EXPECTED_LISTING} | received: unparsable JSON ({error}) "
                f"near {text[position:position + 80]!r}"
            ) from error
        pages.append(page)


def parse_timestamp(value):
    if not isinstance(value, str):
        raise ValueError(value)
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def parse_entry(raw):
    """Validate one listing entry; the selection is unsafe on data it cannot trust."""
    problem = None
    if not isinstance(raw, dict):
        problem = "not an object"
    else:
        for field, kind in (("id", int), ("key", str), ("ref", str), ("size_in_bytes", int)):
            value = raw.get(field)
            if not isinstance(value, kind) or isinstance(value, bool):
                problem = f"{field} is {value!r}"
                break
        if problem is None and raw["id"] < 1:
            problem = f"id is {raw['id']!r}"
        if problem is None and raw["size_in_bytes"] < 0:
            problem = f"size_in_bytes is {raw['size_in_bytes']!r}"
        if problem is None:
            try:
                raw = {**raw, "created": parse_timestamp(raw.get("created_at"))}
            except ValueError:
                problem = f"created_at is {raw.get('created_at')!r}"
    if problem:
        raise CacheListingError(
            f"expected {EXPECTED_LISTING} | received: {raw!r} ({problem})"
        )
    return raw


def parse_listing(text):
    entries = []
    for page in parse_pages(text):
        caches = page.get("actions_caches") if isinstance(page, dict) else None
        if not isinstance(caches, list):
            raise CacheListingError(f"expected {EXPECTED_LISTING} | received: {page!r}")
        entries.extend(parse_entry(raw) for raw in caches)
    return entries


def select(entries):
    """Split a listing into families and the skipped counts.

    Returns (families, other_ref, foreign_key). `families` maps a family name to
    (kept, deleted) lists, both ordered newest first.
    """
    grouped = {}
    other_ref = 0
    foreign_key = 0
    for entry in entries:
        if entry["ref"] != MAIN_REF:
            other_ref += 1
            continue
        match = KEY_GRAMMAR.match(entry["key"])
        if not match:
            foreign_key += 1
            continue
        grouped.setdefault(match.group("family"), []).append(entry)
    families = {}
    for family, members in sorted(grouped.items()):
        members.sort(key=lambda entry: (entry["created"], entry["id"]), reverse=True)
        newest = members[0]["created"]
        kept = [entry for entry in members if entry["created"] == newest]
        deleted = [entry for entry in members if entry["created"] < newest]
        families[family] = (kept, deleted)
    return families, other_ref, foreign_key


def mib(size):
    return f"{size / 1048576:.1f} MiB"


def report(families, other_ref, foreign_key):
    kept_count = deleted_count = deleted_bytes = kept_bytes = 0
    for family, (kept, deleted) in families.items():
        print(f"{family}: keep {len(kept)}, delete {len(deleted)}")
        for label, group in (("keep", kept), ("delete", deleted)):
            for entry in group:
                print(
                    f"  {label:6} {entry['id']} {mib(entry['size_in_bytes']):>10} "
                    f"{entry['created_at']} {entry['key']}"
                )
        kept_count += len(kept)
        kept_bytes += sum(entry["size_in_bytes"] for entry in kept)
        deleted_count += len(deleted)
        deleted_bytes += sum(entry["size_in_bytes"] for entry in deleted)
    print(
        f"summary: {len(families)} families, keep {kept_count} ({mib(kept_bytes)}), "
        f"delete {deleted_count} ({mib(deleted_bytes)}); left alone: {other_ref} on other refs, "
        f"{foreign_key} on {MAIN_REF} outside the rust-cache key grammar"
    )


def run_gh(arguments):
    return subprocess.run(["gh", *arguments], capture_output=True, text=True, check=False)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repo", required=True, help="OWNER/REPO whose caches to prune")
    parser.add_argument(
        "--delete", action="store_true", help="delete the selected caches; the default is a dry run"
    )
    options = parser.parse_args(argv)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", options.repo):
        print(f"expected --repo OWNER/REPO | received: {options.repo!r}", file=sys.stderr)
        return 2

    listed = run_gh(["api", "--paginate", f"repos/{options.repo}/actions/caches?per_page=100"])
    if listed.returncode != 0:
        print(
            f"gh api cache listing for {options.repo} exited {listed.returncode}: "
            f"{listed.stderr.strip()}",
            file=sys.stderr,
        )
        return 1
    try:
        entries = parse_listing(listed.stdout)
    except CacheListingError as error:
        print(error, file=sys.stderr)
        return 1

    families, other_ref, foreign_key = select(entries)
    report(families, other_ref, foreign_key)
    selected = [entry for _, deleted in families.values() for entry in deleted]
    if not options.delete:
        print("dry run: nothing deleted; pass --delete to remove the entries marked delete")
        return 0

    failed = 0
    for entry in selected:
        result = run_gh(
            ["api", "--method", "DELETE", f"repos/{options.repo}/actions/caches/{entry['id']}"]
        )
        if result.returncode != 0:
            failed += 1
            print(
                f"delete of cache {entry['id']} ({entry['key']}) exited {result.returncode}: "
                f"{result.stderr.strip()}",
                file=sys.stderr,
            )
    print(f"deleted {len(selected) - failed} of {len(selected)} caches")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
