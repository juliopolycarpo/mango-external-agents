#!/usr/bin/env bash
# Tests for scripts/prune-rust-caches.py. Every case runs the real script against a named fake `gh`
# (scripts/test-fixtures/fake-gh-cache-cli.sh) that serves a canned cache listing and logs calls, so
# nothing reaches GitHub.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
prune="$repo_root/scripts/prune-rust-caches.py"
work=$(mktemp -d "${TMPDIR:-/tmp}/mea-prune-rust-caches-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

mkdir -p "$work/bin"
cp "$repo_root/scripts/test-fixtures/fake-gh-cache-cli.sh" "$work/bin/gh"
export PATH="$work/bin:$PATH"
export FAKE_GH_LOG="$work/gh.log"
export FAKE_GH_LISTING="$work/listing.json"

fail() {
  echo "test failed: $*" >&2
  exit 1
}

# cache_entry ID KEY REF SIZE CREATED_AT -> one listing entry as JSON.
cache_entry() {
  jq -n -c --argjson id "$1" --arg key "$2" --arg ref "$3" --argjson size "$4" --arg created "$5" \
    '{id: $id, key: $key, ref: $ref, version: "v", size_in_bytes: $size, created_at: $created}'
}

# write_listing ENTRY... -> a two-page `--paginate` body: the first entry alone, then the rest.
write_listing() {
  local first=$1
  shift
  {
    jq -n -c --argjson e "$first" '{total_count: 0, actions_caches: [$e]}'
    jq -n -c '$ARGS.positional | map(fromjson) | {total_count: 0, actions_caches: .}' --args "$@"
  } > "$FAKE_GH_LISTING"
}

# run_prune ARG... -> runs the script, leaving stdout, stderr and the status for the assertions.
run_prune() {
  : > "$FAKE_GH_LOG"
  status=0
  python3 -I "$prune" --repo owner/repository "$@" > "$work/out" 2> "$work/err" || status=$?
}

assert_status() {
  [ "$status" -eq "$1" ] || fail "expected exit status $1 | received: $status; stderr: $(cat "$work/err")"
}

# assert_labelled LABEL ID -> the report has a line starting with the label and carrying the id.
assert_labelled() {
  local label=$1 id=$2
  grep -Eq "^ +$label +$id " "$work/out" || fail "expected '$label $id' in the report | received: $(cat "$work/out")"
}

assert_unlisted() {
  local id=$1
  if grep -Eq "^ +(keep|delete) +$id " "$work/out"; then
    fail "expected cache $id left out of the report | received: $(cat "$work/out")"
  fi
}

main=refs/heads/main
mib=1048576

# Generations of one family: newest kept, the two older ones selected; the two-page listing proves
# the script reads every page. 4 is the sole entry of its family.
write_listing \
  "$(cache_entry 1 v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3 "$main" "$((10 * mib))" 2026-11-15T02:13:07Z)" \
  "$(cache_entry 2 v0-rust-rust-Linux-x64-7543b454-69953215 "$main" "$((20 * mib))" 2026-11-14T19:56:56Z)" \
  "$(cache_entry 3 v0-rust-rust-Linux-x64-7543b454-373a4cb6 "$main" "$((30 * mib))" 2026-11-14T05:04:48Z)" \
  "$(cache_entry 4 v0-rust-fresh-Linux-x64-11111111-22222222 "$main" "$((40 * mib))" 2026-11-10T00:00:00Z)"
run_prune
assert_status 0
assert_labelled keep 1
assert_labelled delete 2
assert_labelled delete 3
assert_labelled keep 4
grep -q 'summary: 2 families, keep 2 (50.0 MiB), delete 2 (50.0 MiB)' "$work/out" ||
  fail "expected the summary with sizes | received: $(cat "$work/out")"

# Dry run, the default: only the listing is requested.
[ "$(wc -l < "$FAKE_GH_LOG")" -eq 1 ] || fail "expected one gh call in a dry run | received: $(cat "$FAKE_GH_LOG")"
grep -q DELETE "$FAKE_GH_LOG" && fail "expected no DELETE in a dry run | received: $(cat "$FAKE_GH_LOG")"
grep -q 'dry run: nothing deleted' "$work/out" || fail "expected the dry-run notice | received: $(cat "$work/out")"

# Delete mode: DELETE for exactly the selected ids (2 and 3), after the one listing call.
run_prune --delete
assert_status 0
expected_log=$(printf '%s\n' \
  'api --paginate repos/owner/repository/actions/caches?per_page=100' \
  'api --method DELETE repos/owner/repository/actions/caches/2' \
  'api --method DELETE repos/owner/repository/actions/caches/3')
[ "$(cat "$FAKE_GH_LOG")" = "$expected_log" ] ||
  fail "expected the listing then DELETE of caches 2 and 3 | received: $(cat "$FAKE_GH_LOG")"
grep -q 'deleted 2 of 2 caches' "$work/out" || fail "expected the deleted count | received: $(cat "$work/out")"

# A failed delete is reported with its id, the others still run, and the exit status is 1.
FAKE_GH_DELETE_FAILS_FOR=2 run_prune --delete
assert_status 1
grep -q 'delete of cache 2 (v0-rust-rust-Linux-x64-7543b454-69953215)' "$work/err" ||
  fail "expected the failed delete named in stderr | received: $(cat "$work/err")"
grep -q 'caches/3$' "$FAKE_GH_LOG" || fail "expected cache 3 still deleted | received: $(cat "$FAKE_GH_LOG")"

# Entries that are never touched, with an older sibling in the main family so the family is real:
# a pull request ref (even the newest of its family), a key outside the grammar, a key with a
# non-hex hash, and a key with a different prefix.
write_listing \
  "$(cache_entry 10 v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3 "$main" "$mib" 2026-11-15T00:00:00Z)" \
  "$(cache_entry 11 v0-rust-rust-Linux-x64-7543b454-69953215 "$main" "$mib" 2026-11-14T00:00:00Z)" \
  "$(cache_entry 12 v0-rust-rust-Linux-x64-0dc81168-373a4cb6 refs/pull/497/merge "$mib" 2026-11-01T00:00:00Z)" \
  "$(cache_entry 13 Linux-cargo-registry-abc "$main" "$mib" 2026-11-01T00:00:00Z)" \
  "$(cache_entry 14 v0-rust-rust-Linux-x64-zzzzzzzz-69953215 "$main" "$mib" 2026-11-01T00:00:00Z)" \
  "$(cache_entry 15 v1-rust-rust-Linux-x64-7543b454-373a4cb6 "$main" "$mib" 2026-11-01T00:00:00Z)" \
  "$(cache_entry 16 v0-rust-rust-FreeBSD-x64-7543b454-373a4cb6 "$main" "$mib" 2026-11-01T00:00:00Z)"
run_prune --delete
assert_status 0
assert_labelled delete 11
for id in 12 13 14 15 16; do assert_unlisted "$id"; done
grep -q 'caches/11$' "$FAKE_GH_LOG" || fail "expected cache 11 deleted | received: $(cat "$FAKE_GH_LOG")"
[ "$(grep -c DELETE "$FAKE_GH_LOG")" -eq 1 ] || fail "expected exactly one DELETE | received: $(cat "$FAKE_GH_LOG")"
grep -q 'left alone: 1 on other refs, 4 on refs/heads/main outside' "$work/out" ||
  fail "expected the left-alone counts | received: $(cat "$work/out")"

# A pull request ref that is the only holder of an old generation is not pruned and does not
# displace main's newest: main keeps its own newest entry.
write_listing \
  "$(cache_entry 20 v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3 refs/pull/1/merge "$mib" 2026-11-16T00:00:00Z)" \
  "$(cache_entry 21 v0-rust-rust-Linux-x64-7543b454-69953215 "$main" "$mib" 2026-11-14T00:00:00Z)"
run_prune --delete
assert_status 0
assert_labelled keep 21
assert_unlisted 20
grep -q DELETE "$FAKE_GH_LOG" && fail "expected no DELETE when main has one entry | received: $(cat "$FAKE_GH_LOG")"

# OS names with an underscore and names with dashes resolve to their own families: Windows_NT,
# `msrv-windows` (shared prefix and job id), job-id-suffixed names, and arm64.
write_listing \
  "$(cache_entry 30 v0-rust-msrv-windows-msrv-windows-Windows_NT-x64-538b5e99-6eb708d3 "$main" "$mib" 2026-11-15T02:04:13Z)" \
  "$(cache_entry 31 v0-rust-msrv-windows-msrv-windows-Windows_NT-x64-79c091f4-69953215 "$main" "$mib" 2026-11-14T19:46:42Z)" \
  "$(cache_entry 32 v0-rust-rust-Windows_NT-x64-b7365419-6eb708d3 "$main" "$mib" 2026-11-15T02:17:47Z)" \
  "$(cache_entry 33 v0-rust-rust-Windows_NT-x64-8c623fa0-69953215 "$main" "$mib" 2026-11-14T20:07:16Z)" \
  "$(cache_entry 34 v0-rust-msrv-msrv-Linux-x64-2575483e-6eb708d3 "$main" "$mib" 2026-11-15T02:06:05Z)" \
  "$(cache_entry 35 v0-rust-msrv-msrv-Linux-x64-cd1ba6af-69953215 "$main" "$mib" 2026-11-14T19:51:31Z)" \
  "$(cache_entry 36 v0-rust-rust-Darwin-arm64-80373fc8-6eb708d3 "$main" "$mib" 2026-11-15T02:15:23Z)" \
  "$(cache_entry 37 v0-rust-rust-Darwin-arm64-0dc81168-69953215 "$main" "$mib" 2026-11-14T20:06:01Z)"
run_prune
assert_status 0
for id in 30 32 34 36; do assert_labelled keep "$id"; done
for id in 31 33 35 37; do assert_labelled delete "$id"; done
grep -q '^v0-rust-msrv-windows-msrv-windows-Windows_NT-x64: keep 1, delete 1' "$work/out" ||
  fail "expected the msrv-windows family on Windows_NT | received: $(cat "$work/out")"
grep -q 'summary: 4 families' "$work/out" || fail "expected 4 families | received: $(cat "$work/out")"

# Entries tied for the newest created_at are all kept: their order is unknown.
write_listing \
  "$(cache_entry 40 v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3 "$main" "$mib" 2026-11-15T00:00:00Z)" \
  "$(cache_entry 41 v0-rust-rust-Linux-x64-7543b454-69953215 "$main" "$mib" 2026-11-15T00:00:00Z)" \
  "$(cache_entry 42 v0-rust-rust-Linux-x64-0dc81168-373a4cb6 "$main" "$mib" 2026-11-14T00:00:00Z)"
run_prune
assert_status 0
assert_labelled keep 40
assert_labelled keep 41
assert_labelled delete 42

# Malformed listings fail naming the received value and the expected shape, and delete nothing.
expect_malformed() {
  local label=$1 received=$2
  run_prune --delete
  assert_status 1
  grep -q 'expected {"actions_caches"' "$work/err" ||
    fail "($label) expected the shape in stderr | received: $(cat "$work/err")"
  grep -qF "$received" "$work/err" ||
    fail "($label) expected stderr to name received value $received | received: $(cat "$work/err")"
  grep -q DELETE "$FAKE_GH_LOG" && fail "($label) expected no DELETE | received: $(cat "$FAKE_GH_LOG")"
  return 0
}
printf '%s\n' '{"message": "Not Found"}' > "$FAKE_GH_LISTING"
expect_malformed 'no actions_caches' "'message': 'Not Found'"
printf '%s\n' '{"actions_caches": [{"id": "7", "key": "v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3", "ref": "refs/heads/main", "size_in_bytes": 1, "created_at": "2026-11-15T00:00:00Z"}]}' > "$FAKE_GH_LISTING"
expect_malformed 'string id' "id is '7'"
printf '%s\n' '{"actions_caches": [{"id": 7, "key": "v0-rust-rust-Linux-x64-a5cc1cd7-6eb708d3", "ref": "refs/heads/main", "size_in_bytes": 1, "created_at": "yesterday"}]}' > "$FAKE_GH_LISTING"
expect_malformed 'bad timestamp' "created_at is 'yesterday'"
printf '%s\n' '{"actions_caches": [' > "$FAKE_GH_LISTING"
expect_malformed 'truncated JSON' 'unparsable JSON'

# A failing listing call is reported with its status and stops before any delete.
rm -f "$FAKE_GH_LISTING"
run_prune --delete
assert_status 1

# Argument errors.
status=0
python3 -I "$prune" --repo 'not a repo' > "$work/out" 2> "$work/err" || status=$?
assert_status 2
grep -q "received: 'not a repo'" "$work/err" || fail "expected the received repo in stderr | received: $(cat "$work/err")"

echo "prune-rust-caches: all cases passed"
