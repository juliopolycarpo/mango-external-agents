#!/usr/bin/env bash
# Regression coverage for scripts/check-bench-runner.sh: identical copies pass; a differing copy or
# a missing file is rejected with the expected/received message, and every tracked runner copy is
# one the default check lists. The default benchmark command also covers every registered target
# and forwards case filters unchanged.
set -euo pipefail
cd "$(dirname "$0")/.."

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

printf 'runner\n' > "$scratch/a.rs"
printf 'runner\n' > "$scratch/same.rs"
printf 'runner, edited\n' > "$scratch/drifted.rs"

scripts/check-bench-runner.sh >/dev/null
scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/same.rs" >/dev/null

if scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/drifted.rs" >/dev/null 2>"$scratch/drift.err"; then
  echo "expected rejection of a drifted runner copy, received success" >&2
  exit 1
fi
grep -q 'to be identical, received a difference' "$scratch/drift.err" || {
  echo "expected the drift message to say the copies differ, received: $(cat "$scratch/drift.err")" >&2
  exit 1
}

# Every copy is compared, not only the first: a drifted third copy is rejected too.
if scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/same.rs" "$scratch/drifted.rs" >/dev/null 2>"$scratch/third.err"; then
  echo "expected rejection of a drifted third runner copy, received success" >&2
  exit 1
fi
grep -q 'drifted.rs to be identical, received a difference' "$scratch/third.err" || {
  echo "expected the drift message to name the third copy, received: $(cat "$scratch/third.err")" >&2
  exit 1
}

if scripts/check-bench-runner.sh "$scratch/a.rs" >/dev/null 2>"$scratch/lonely.err"; then
  echo "expected rejection of a single runner path, received success" >&2
  exit 1
fi
grep -q 'a reference runner and at least one copy' "$scratch/lonely.err" || {
  echo "expected the single-path message, received: $(cat "$scratch/lonely.err")" >&2
  exit 1
}

if scripts/check-bench-runner.sh "$scratch/a.rs" "$scratch/missing.rs" >/dev/null 2>"$scratch/missing.err"; then
  echo "expected rejection of a missing runner copy, received success" >&2
  exit 1
fi
grep -q 'received no such file' "$scratch/missing.err" || {
  echo "expected the missing-file message, received: $(cat "$scratch/missing.err")" >&2
  exit 1
}
# Every runner copy in the tree is one the default check compares. A crate that grows benches and a
# copy of the runner, but is never added to the default list, would drift without a failure. Only a
# line of the list itself counts: the path on its own, optionally continued with a backslash, so a
# comment or a message that mentions the path does not.
for runner in $(git ls-files ':(glob)**/benches/support/mod.rs'); do
  grep -qE "^[[:space:]]+${runner//./\\.}( \\\\)?\$" scripts/check-bench-runner.sh || {
    echo "expected scripts/check-bench-runner.sh to compare $runner, received a runner copy it never lists" >&2
    exit 1
  }
done

# The default command promises all registered benchmark targets. Inspect actual Cargo argv with
# a named fake executor, and derive the expected targets from metadata instead of another list.
cargo metadata --locked --no-deps --format-version 1 > "$scratch/metadata.json"
mkdir -p "$scratch/bin"
cp scripts/test-fixtures/fake-bench-cargo.sh "$scratch/bin/cargo"
PATH="$scratch/bin:$PATH" FAKE_BENCH_CARGO_LOG="$scratch/cargo.jsonl" \
  scripts/bench.sh fixture-filter > "$scratch/bench.out"
python3 - "$scratch/metadata.json" "$scratch/cargo.jsonl" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    packages = json.load(source)["packages"]
declared = {
    (package["name"], target["name"])
    for package in packages
    for target in package["targets"]
    if "bench" in target["kind"]
}
executed = set()
with open(sys.argv[2], encoding="utf-8") as log:
    for line in log:
        argv = json.loads(line)
        if not argv or argv[0] != "bench":
            continue
        boundary = argv.index("--") if "--" in argv else len(argv)
        flags = argv[1:boundary]
        received_filter = argv[boundary + 1:]
        if received_filter != ["fixture-filter"]:
            raise SystemExit(f"expected benchmark filter fixture-filter, received {received_filter!r}")
        selected_packages = {
            flags[index + 1] for index, value in enumerate(flags)
            if value in ("-p", "--package")
        }
        selected_benches = {
            flags[index + 1] for index, value in enumerate(flags)
            if value == "--bench"
        }
        executed.update(
            (package, bench) for package, bench in declared
            if (not selected_packages or package in selected_packages)
            and (not selected_benches or bench in selected_benches)
        )
if declared != executed:
    missing = sorted(f"{package}/{bench}" for package, bench in declared - executed)
    raise SystemExit(f"expected scripts/bench.sh to run every registered benchmark, received missing {missing}")
PY
echo "bench runner checks passed"
