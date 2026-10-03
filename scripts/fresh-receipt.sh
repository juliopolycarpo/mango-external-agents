#!/usr/bin/env bash
# Evidence for the advisory fresh-dependencies run: the exact lock it resolved and what happened
# to it afterwards. `snapshot` runs right after `cargo update` and keeps the lock; `finish` runs
# whatever the gates did and writes receipt.json beside it.
#
# The receipt separates the three ways the run can fail, which a red job alone cannot:
#   resolution-failed  `cargo update` itself failed; no fresh lock exists and none is kept
#                      (a run cancelled during resolution is `cancelled`, not this)
#   gates-failed       a fresh lock exists and at least one gate after it failed
#   lock-mutated       the gates changed Cargo.lock after the snapshot, so the kept lock is not
#                      the graph that was tested
# or `cancelled` (no gate failed, but some did not finish: cancelled, or skipped because the run
# was cancelled before they started) / `passed`. Gate steps are the workflow steps whose id starts with `gate_`; the
# step with id `resolve` is the resolution.
#
# Environment: RECEIPT_DIR (required); UPDATE_LOG (cargo update output); finish also reads
# STEPS_JSON (the `steps` context as JSON), SOURCE_SHA, HEAD_SHA, EVENT_NAME, REF, RUN_ID,
# RUN_ATTEMPT, REPOSITORY and MSRV_TOOLCHAIN, and appends a summary to GITHUB_STEP_SUMMARY.
# FRESH_ROOT overrides the directory holding Cargo.lock (default: the repository root; the test
# uses a scratch repository).
#
# Usage: RECEIPT_DIR=out UPDATE_LOG=cargo-update.log scripts/fresh-receipt.sh snapshot
set -euo pipefail
cd "${FRESH_ROOT:-$(dirname "$0")/..}"

mode=${1:-}
case "$mode" in
  snapshot | finish) ;;
  *) echo "expected mode snapshot or finish, received '$mode'" >&2; exit 2 ;;
esac
: "${RECEIPT_DIR:?expected RECEIPT_DIR, the directory the receipt is written to}"

# sha256sum is GNU coreutils; macOS ships shasum instead (see scripts/bench.sh). Reads stdin.
sha256_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | cut -d' ' -f1
  else
    echo "expected sha256sum or shasum on PATH to digest Cargo.lock, received neither" >&2
    return 1
  fi
}
lock_hash() { sha256_stdin < "$1"; }

snapshot() {
  mkdir -p "$RECEIPT_DIR"
  cp Cargo.lock "$RECEIPT_DIR/Cargo.lock"
  lock_hash Cargo.lock > "$RECEIPT_DIR/Cargo.lock.sha256"
  # The committed lock this run started from, so a reader can tell whether the graph moved at all.
  git show HEAD:Cargo.lock | sha256_stdin > "$RECEIPT_DIR/committed-Cargo.lock.sha256"
  if [ -n "${UPDATE_LOG:-}" ] && [ -f "$UPDATE_LOG" ]; then
    cp "$UPDATE_LOG" "$RECEIPT_DIR/cargo-update.log"
  fi
}

toolchain_report() {
  echo "### default toolchain (rust-toolchain.toml)"
  rustc -Vv
  cargo -V
  if [ -n "${MSRV_TOOLCHAIN:-}" ]; then
    echo
    echo "### minimum toolchain $MSRV_TOOLCHAIN"
    rustc "+$MSRV_TOOLCHAIN" -Vv 2>&1 || echo "not installed"
  fi
}

finish() {
  : "${STEPS_JSON:?expected STEPS_JSON, the workflow steps context as JSON}"
  mkdir -p "$RECEIPT_DIR"
  local resolution lock_present=false resolved_hash='' committed_hash='' live_hash='' stable=null
  resolution=$(jq -r '.resolve.outcome // "missing"' <<< "$STEPS_JSON")

  if [ "$resolution" = success ] && [ -f "$RECEIPT_DIR/Cargo.lock" ]; then
    lock_present=true
    resolved_hash=$(cat "$RECEIPT_DIR/Cargo.lock.sha256")
    committed_hash=$(cat "$RECEIPT_DIR/committed-Cargo.lock.sha256")
    live_hash=$(lock_hash Cargo.lock)
    stable=false
    [ "$live_hash" = "$resolved_hash" ] && stable=true
  else
    # No fresh lock was produced. Whatever sits in the directory is not a resolved graph.
    rm -f "$RECEIPT_DIR/Cargo.lock" "$RECEIPT_DIR/Cargo.lock.sha256" "$RECEIPT_DIR/committed-Cargo.lock.sha256"
  fi

  toolchain_report > "$RECEIPT_DIR/rustc-Vv.txt" 2>&1 || true
  # The resolver's own account of which Rust version it resolved for, and what it held back.
  local policy held
  policy=$(grep -E '^\s*Locking [0-9]+ packages to ' "${UPDATE_LOG:-/dev/null}" 2>/dev/null | head -n 1 | sed 's/^\s*//' || true)
  held=$(grep -E 'requires Rust' "${UPDATE_LOG:-/dev/null}" 2>/dev/null | sed 's/^\s*//' || true)

  jq -n \
    --argjson steps "$STEPS_JSON" \
    --arg resolution "$resolution" \
    --argjson lock_present "$lock_present" \
    --arg resolved_hash "$resolved_hash" \
    --arg committed_hash "$committed_hash" \
    --argjson stable "$stable" \
    --arg policy "$policy" \
    --arg held "$held" \
    --arg incompatible "${CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS:-}" \
    --arg source_sha "${SOURCE_SHA:-}" \
    --arg head_sha "${HEAD_SHA:-}" \
    --arg event "${EVENT_NAME:-}" \
    --arg ref "${REF:-}" \
    --arg run_id "${RUN_ID:-}" \
    --arg run_attempt "${RUN_ATTEMPT:-}" \
    --arg repository "${REPOSITORY:-}" '
    ($steps | to_entries | map(select(.key | startswith("gate_")))
      | map({key: (.key | ltrimstr("gate_")), value: .value.outcome}) | from_entries) as $gates
    | ([$gates[]] | map(select(. != "success"))) as $not_passed
    | {
        schema: 1,
        result: (
          if $resolution == "cancelled" then "cancelled"
          elif $resolution != "success" then "resolution-failed"
          elif $stable == false then "lock-mutated"
          elif ([$gates[]] | any(. == "failure")) then "gates-failed"
          elif ($not_passed | length) > 0 then "cancelled"
          else "passed" end),
        resolution: $resolution,
        gates: $gates,
        lock: (if $lock_present then {
          file: "Cargo.lock",
          sha256: $resolved_hash,
          committed_sha256: $committed_hash,
          moved_from_committed: ($resolved_hash != $committed_hash),
          unchanged_by_gates: $stable
        } else null end),
        resolver: {
          log_line: (if $policy == "" then null else $policy end),
          held_back_by_rust_version: ($held | split("\n") | map(select(. != ""))),
          incompatible_rust_versions_env: (if $incompatible == "" then null else $incompatible end)
        },
        source: {
          repository: $repository,
          sha: $source_sha,
          head_sha: (if $head_sha == "" then $source_sha else $head_sha end),
          ref: $ref,
          event: $event,
          run_id: $run_id,
          run_attempt: $run_attempt
        }
      }' > "$RECEIPT_DIR/receipt.json"

  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo '### Fresh run receipt'
      echo
      echo "Result: **$(jq -r .result "$RECEIPT_DIR/receipt.json")** (resolution: $resolution)"
      echo
      echo '| Gate | Outcome |'
      echo '| --- | --- |'
      jq -r '.gates | to_entries[] | "| \(.key) | \(.value) |"' "$RECEIPT_DIR/receipt.json"
      echo
      if [ "$lock_present" = true ]; then
        echo "Resolved lock sha256: \`$resolved_hash\` (committed: \`$committed_hash\`)"
      else
        echo 'No fresh lock was produced.'
      fi
    } >> "$GITHUB_STEP_SUMMARY"
  fi
  jq . "$RECEIPT_DIR/receipt.json"
}

"$mode"
