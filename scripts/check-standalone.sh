#!/usr/bin/env bash
# Compile and test the registry consumers outside every repository workspace.
#
#   current     tests/standalone-current: the published release the workspace last shipped (or the
#               one before it, until the post-release bump; see scripts/check-consumer-pin.sh).
#   historical  tests/standalone-historical: a fixed =0.3.1 control on its own, older minimum
#               compiler. It is not evidence for any later release or for this checkout.
#
# Each consumer is copied to a temporary directory and built from there, but rustup picks the
# compiler from the working directory, which is the repository root: the pinned toolchain in
# rust-toolchain.toml applies unless `RUSTUP_TOOLCHAIN` is set. A job that needs a consumer's own
# minimum sets `RUSTUP_TOOLCHAIN` and passes `--test-only`, because the minimal toolchain profile
# carries neither rustfmt nor clippy.
#
# The consumer's declared `rust-version` must be what the pinned release declares on the registry,
# so a consumer cannot claim a minimum its crates do not.
#
# A consumer's result depends only on its own directory, the pinned toolchain and this script, never
# on the workspace sources: `--changed-since <rev>` skips the build of a consumer whose inputs are
# the same in the working tree as at <rev>. The fixture comparison and the pin shape are checked
# either way, since they read the checkout. A <rev> this clone does not have builds, so a shallow
# clone or a first push can only err towards building. `--also-input <path>` adds an input, for a
# caller whose compiler comes from somewhere else (the MSRV lane reads it from Cargo.toml). CI
# passes the flag on pull requests and pushes and omits it on its weekly run, which is what
# notices a runner image or registry change; so does a bare local call.
#
# Usage: scripts/check-standalone.sh [--test-only] [--changed-since <rev>] [--also-input <path>]...
#                                    [current|historical|all]   # default: all
set -euo pipefail
cd "$(dirname "$0")/.."

usage='expected --test-only, --changed-since <rev>, --also-input <path>, current, historical or all'
test_only=false
selection=all
changed_since=''
shared_inputs=(rust-toolchain.toml scripts/check-standalone.sh)
while [ $# -gt 0 ]; do
  case "$1" in
    --test-only) test_only=true ;;
    --changed-since | --also-input)
      if [ $# -lt 2 ] || [ -z "$2" ]; then
        echo "expected a value after $1, received none ($usage)" >&2
        exit 2
      fi
      if [ "$1" = --changed-since ]; then changed_since=$2; else shared_inputs+=("$2"); fi
      shift
      ;;
    current | historical | all) selection=$1 ;;
    *) echo "unknown argument: $1 ($usage)" >&2; exit 2 ;;
  esac
  shift
done

scratch=$(mktemp -d "${TMPDIR:-/tmp}/independent-agent-host.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
target_base="${CARGO_TARGET_DIR:-$PWD/target}"

# inputs_unchanged NAME -> success when --changed-since was given and no input of the consumer
# differs between that commit and the working tree, untracked files included.
inputs_unchanged() {
  local inputs=("tests/standalone-$1" "${shared_inputs[@]}")
  [ -n "$changed_since" ] || return 1
  if ! git rev-parse --verify --quiet "$changed_since^{commit}" >/dev/null; then
    echo "· $1 registry consumer: $changed_since is not a commit in this clone, so its inputs cannot be compared; building"
    return 1
  fi
  git diff --quiet "$changed_since" -- "${inputs[@]}" || return 1
  [ -z "$(git ls-files --others --exclude-standard -- "${inputs[@]}")" ]
}

check_consumer() {
  local name=$1 source="tests/standalone-$1" pin consumer declared
  cmp fixtures/claude/help/2.1.260.txt "$source/tests/fixtures/claude-help.txt"
  cmp fixtures/claude/transcripts/read-turn.jsonl "$source/tests/fixtures/claude-turn.jsonl"
  pin=$(sed -n 's/^mango-external-agents = { version = "=\([^"]*\)".*/\1/p' "$source/Cargo.toml" | sort -u)
  if [ -z "$pin" ] || [ "$(printf '%s\n' "$pin" | grep -c .)" -ne 1 ]; then
    # The entries as written: a requirement that is not an exact pin matches nothing above.
    declared=$(grep '^mango-external-agents = ' "$source/Cargo.toml" | paste -sd';' - || true)
    echo "$source/Cargo.toml: expected one exact '=x.y.z' mango-external-agents pin, received: ${declared:-no mango-external-agents entry}" >&2
    return 1
  fi

  if inputs_unchanged "$name"; then
    echo "✓ $name registry consumer (=$pin): inputs unchanged since $changed_since ($source ${shared_inputs[*]}); not rebuilt"
    return 0
  fi

  consumer="$scratch/$name"
  mkdir "$consumer"
  cp "$source"/{Cargo.toml,Cargo.lock,README.md} "$consumer/"
  cp -R "$source"/{src,tests} "$consumer/"
  export CARGO_TARGET_DIR="$target_base/standalone-$name"

  echo "▶ $name registry consumer (=$pin) outside workspace: $consumer"
  rustc --version
  cargo metadata --manifest-path "$consumer/Cargo.toml" --locked --format-version 1 > "$consumer/metadata.json"
  python3 - "$consumer/metadata.json" "$pin" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    metadata = json.load(source)
pin = sys.argv[2]
expected = {"mango-external-agents", "mango-agent-acp", "mango-agent-claude", "mango-agent-codex"}
packages = {package["name"]: package for package in metadata["packages"]}
for name in sorted(expected):
    package = packages[name]
    assert package["version"] == pin, f"{name}: expected {pin}, received {package['version']}"
    assert package["source"] and package["source"].startswith("registry+"), f"{name}: expected registry source, received {package['source']}"
    print(f"{name} = {package['version']} ({package['source']}), rust-version {package['rust_version']}")
for name in packages:
    assert not name.startswith("mangostudio") and name != "mango-protocol", f"expected no product dependency, received {name}"
    assert name not in {"aws-lc-rs", "openssl", "openssl-sys"}, f"expected ring-only TLS dependencies, received {name}"
root = packages["independent-agent-host"]
for name in sorted(expected):
    assert root["rust_version"] == packages[name]["rust_version"], f"expected consumer rust-version {packages[name]['rust_version']} (declared by {name} {pin}), received {root['rust_version']}"
assert metadata["workspace_members"] == [root["id"]], "expected only the independent consumer in its workspace"
for dependency in root["dependencies"]:
    if dependency["name"] in expected:
        assert dependency["req"] == f"={pin}" and not dependency.get("path"), f"expected exact registry dependency ={pin}, received {dependency}"
PY

  if [ "$test_only" = false ]; then
    cargo fmt --manifest-path "$consumer/Cargo.toml" -- --check
  fi
  cargo test --manifest-path "$consumer/Cargo.toml" --no-default-features --locked
  cargo test --manifest-path "$consumer/Cargo.toml" --all-features --locked
  if [ "$test_only" = false ]; then
    cargo clippy --manifest-path "$consumer/Cargo.toml" --all-targets --all-features --locked -- -D warnings
  fi
  echo "✓ $name registry consumer passed"
}

case "$selection" in
  current) check_consumer current ;;
  historical) check_consumer historical ;;
  all)
    check_consumer current
    check_consumer historical
    ;;
esac
echo "✓ independent registry consumer checks passed"
