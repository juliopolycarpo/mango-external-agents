#!/usr/bin/env bash
# Focused tests for the vendor drift helpers. No test contacts a vendor or GitHub.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/mea-vendor-drift-test.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT

# shellcheck source=install-vendor-cli.sh
source "$repo_root/scripts/install-vendor-cli.sh"
# shellcheck source=check-vendor-contract.sh
source "$repo_root/scripts/check-vendor-contract.sh"
# shellcheck source=write-vendor-drift-report.sh
source "$repo_root/scripts/write-vendor-drift-report.sh"
# shellcheck source=upsert-vendor-drift-issue.sh
source "$repo_root/scripts/upsert-vendor-drift-issue.sh"
# shellcheck source=vendor-codex.sh
source "$repo_root/scripts/vendor-codex.sh"
# shellcheck source=check-claude-bare-reference.sh
source "$repo_root/scripts/check-claude-bare-reference.sh"

fail() {
  echo "test failed: $*" >&2
  exit 1
}

assert_eq() {
  local actual="$1"
  local expected="$2"
  local message="$3"
  if [ "$actual" != "$expected" ]; then
    fail "$message; expected $expected, received $actual"
  fi
}

assert_contains() {
  local path="$1"
  local expected="$2"
  local message="$3"
  if ! grep -Fq -- "$expected" "$path"; then
    fail "$message; expected $expected in $path"
  fi
}

expect_status() {
  local expected="$1"
  shift
  local actual
  set +e
  "$@" >/dev/null 2>&1
  actual=$?
  set -e
  assert_eq "$actual" "$expected" "unexpected command status for $*"
}

test_vendor_release_mapping() {
  assert_eq "$(vendor_repo claude)" 'anthropics/claude-code' 'Claude release repository'
  assert_eq "$(vendor_repo codex)" 'openai/codex' 'Codex release repository'
  assert_eq "$(release_tag codex 0.154.0)" 'rust-v0.154.0' 'Codex pinned release tag'
  assert_eq "$(release_tag claude 2.1.270)" 'v2.1.270' 'Claude pinned release tag'
  assert_eq "$(asset_name codex windows-x64)" 'codex-x86_64-pc-windows-msvc.exe.zip' 'Codex Windows asset'
  assert_eq "$(asset_name claude darwin-arm64)" 'claude-darwin-arm64.tar.gz' 'Claude macOS asset'
  assert_eq "$(asset_name opencode linux-x64)" 'opencode-linux-x64.tar.gz' 'OpenCode Linux asset'
  assert_eq "$(binary_name codex windows-x64)" 'codex.exe' 'Codex installed Windows name'
  assert_eq "$(released_binary_name codex linux-x64 codex-x86_64-unknown-linux-musl.tar.gz)" \
    'codex-x86_64-unknown-linux-musl' 'Codex release binary name'
  assert_eq "$(pinned_archive_digest claude 2.1.270 claude-linux-x64.tar.gz)" \
    'b069b327de3ad6c8cda70886675da46fb213797ac99a367df384e2202b37e0b7' \
    'Claude pinned archive digest'
  expect_status 2 vendor_repo unknown
  expect_status 2 asset_name claude solaris-sparc
}

test_archive_checksum_verification() {
  local archive="$test_root/pinned-archive"
  local error="$test_root/pinned-archive.error"
  local digest
  printf 'the recorded bytes\n' > "$archive"
  digest=$(archive_sha256 "$archive")
  verify_sha256 "$digest" "$archive"
  if verify_sha256 \
    '0000000000000000000000000000000000000000000000000000000000000000' \
    "$archive" > "$error" 2>&1; then
    fail 'a mismatched archive checksum must be rejected'
  fi
  assert_contains "$error" 'expected SHA-256 0000000000000000000000000000000000000000000000000000000000000000' \
    'checksum mismatch shape'
}

test_every_pinned_platform_has_a_checksum() {
  local entry
  local vendor
  local version
  local target
  local asset
  local digest
  for entry in \
    'claude 2.1.270 linux-x64' \
    'claude 2.1.270 linux-arm64' \
    'claude 2.1.270 darwin-x64' \
    'claude 2.1.270 darwin-arm64' \
    'claude 2.1.270 windows-x64' \
    'claude 2.1.270 windows-arm64' \
    'codex 0.154.0 linux-x64' \
    'codex 0.154.0 linux-arm64' \
    'codex 0.154.0 darwin-x64' \
    'codex 0.154.0 darwin-arm64' \
    'codex 0.154.0 windows-x64' \
    'codex 0.154.0 windows-arm64' \
    'opencode 1.18.30 linux-x64' \
    'opencode 1.18.30 linux-arm64' \
    'opencode 1.18.30 darwin-x64' \
    'opencode 1.18.30 darwin-arm64' \
    'opencode 1.18.30 windows-x64' \
    'opencode 1.18.30 windows-arm64'; do
    read -r vendor version target <<EOF
$entry
EOF
    asset=$(asset_name "$vendor" "$target")
    digest=$(pinned_archive_digest "$vendor" "$version" "$asset")
    assert_eq "${#digest}" '64' "pinned digest length for $vendor $target"
  done
}

test_latest_checksum_lookup() {
  local fake_bin="$test_root/latest-gh-bin"
  local original_path="$PATH"
  mkdir -p "$fake_bin"
  ln -s "$repo_root/scripts/test-fixtures/fake-release-cli.sh" "$fake_bin/gh"
  export PATH="$fake_bin:$PATH"
  export FAKE_RELEASE_ASSET='codex-linux.tar.gz'
  export FAKE_RELEASE_DIGEST='d7e18b2597ae8f242f5f31ee9e90deef48dbc9edd634d9868fb6435d08c07f02'
  assert_eq "$(latest_archive_digest openai/codex rust-v0.154.0 "$FAKE_RELEASE_ASSET")" \
    "$FAKE_RELEASE_DIGEST" 'latest release API digest'
  export FAKE_RELEASE_DIGEST=''
  expect_status 2 latest_archive_digest openai/codex rust-v0.154.0 "$FAKE_RELEASE_ASSET"
  unset FAKE_RELEASE_ASSET
  unset FAKE_RELEASE_DIGEST
  export PATH="$original_path"
}

test_codex_output_arguments() {
  parse_arguments 0.154.0 --out /tmp/codex-contract
  assert_eq "$version" '0.154.0' 'Codex inventory version'
  assert_eq "$vendor_dir" '/tmp/codex-contract' 'Codex scratch output directory'
  expect_status 2 parse_arguments
  expect_status 2 parse_arguments 0.154.0 --out
  expect_status 2 parse_arguments 0.154.0 unexpected
}

test_codex_generator_requires_an_installed_cli() {
  local original_path="$PATH"
  PATH='/usr/bin:/bin'
  expect_status 2 generate_inventory
  PATH="$original_path"
}

test_codex_generator_cleans_up_in_a_standalone_process() {
  local fake_bin="$test_root/codex-bin"
  local output="$test_root/codex-output"
  local original_path="$PATH"
  mkdir -p "$fake_bin"
  ln -s "$repo_root/scripts/test-fixtures/fake-codex-schema.sh" "$fake_bin/codex"
  ln -s "$repo_root/scripts/test-fixtures/fake-python3.sh" "$fake_bin/python3"
  export PATH="$fake_bin:$PATH"
  export FAKE_CODEX_SCHEMA="$repo_root/crates/mango-agent-codex/vendor/schema.json"
  "$repo_root/scripts/vendor-codex.sh" 0.154.0 --out "$output" >/dev/null
  assert_contains "$output/schema.json" '"codexVersion": "0.154.0"' \
    'standalone Codex inventory output'
  unset FAKE_CODEX_SCHEMA
  export PATH="$original_path"
}

test_contract_comparison() {
  local committed="$test_root/committed"
  local captured="$test_root/captured"
  mkdir -p "$committed" "$captured"
  printf 'same\n' > "$committed/contract.json"
  cp "$committed/contract.json" "$captured/contract.json"
  compare_contract claude "$committed" "$captured" >/dev/null
  printf 'changed\n' > "$captured/contract.json"
  expect_status 1 compare_contract claude "$committed" "$captured"
  expect_status 2 compare_contract claude "$committed/missing" "$captured"
}

test_drift_report() {
  local committed="$test_root/report-committed"
  local observed="$test_root/report-observed"
  local report="$test_root/report.md"
  mkdir -p "$committed" "$observed"
  printf 'same\n' > "$committed/contract.json"
  cp "$committed/contract.json" "$observed/contract.json"
  write_report claude "$committed" "$observed" "$report"
  if [ -e "$report" ]; then
    fail 'a matching contract must not write a report'
  fi
  printf 'different\n' > "$observed/contract.json"
  expect_status 1 write_report claude "$committed" "$observed" "$report"
  assert_contains "$report" '<!-- vendor-drift:claude -->' 'drift report marker'
  assert_contains "$report" 'diff --git' 'drift report diff'
  expect_status 2 write_report claude "$committed/missing" "$observed" "$report"
  write_failure_report claude 'public contract capture' "$report"
  assert_contains "$report" 'could not complete the public contract capture stage' 'failure report stage'
}

test_claude_bare_reference() {
  local reference="$test_root/claude-headless.html"
  printf '%s\n' 'Without it, <code>claude -p</code> loads the same context as an interactive session.' > "$reference"
  validate_reference "$reference" >/dev/null
  printf '%s\n' 'bare mode is now the default.' > "$reference"
  expect_status 1 validate_reference "$reference"
  expect_status 2 validate_reference "$reference/missing"
}

test_issue_deduplication() {
  local fake_bin="$test_root/fake-bin"
  local report="$test_root/issue.md"
  local log="$test_root/gh.log"
  local original_path="$PATH"
  mkdir -p "$fake_bin"
  ln -s "$repo_root/scripts/test-fixtures/fake-github-cli.sh" "$fake_bin/gh"
  printf '<!-- vendor-drift:claude -->\nreport\n' > "$report"
  export PATH="$fake_bin:$PATH"
  export FAKE_GH_LOG="$log"
  export FAKE_GH_VENDOR='claude'
  export FAKE_GH_ISSUE_NUMBER='42'
  export FAKE_GH_ISSUE_BODY='<!-- vendor-drift:claude -->'

  upsert_issue owner/repository claude "$report" >/dev/null
  assert_contains "$log" 'issue edit 42' 'existing drift issue update'

  : > "$log"
  export FAKE_GH_ISSUE_BODY='manual issue without the drift marker'
  upsert_issue owner/repository claude "$report" >/dev/null
  assert_contains "$log" 'issue create' 'manual issue is not overwritten'

  : > "$log"
  export FAKE_GH_ISSUE_NUMBER=''
  export FAKE_GH_ISSUE_BODY=''
  upsert_issue owner/repository claude "$report" >/dev/null
  assert_contains "$log" 'issue create' 'new drift issue creation'

  export FAKE_GH_FAILURE='issue-list'
  expect_status 2 upsert_issue owner/repository claude "$report"
  unset FAKE_GH_FAILURE

  export FAKE_GH_ISSUE_NUMBER='42'
  export FAKE_GH_ISSUE_BODY='<!-- vendor-drift:claude -->'
  export FAKE_GH_FAILURE='issue-view'
  expect_status 2 upsert_issue owner/repository claude "$report"
  unset FAKE_GH_FAILURE
  export PATH="$original_path"
}

test_vendor_release_mapping
test_archive_checksum_verification
test_every_pinned_platform_has_a_checksum
test_latest_checksum_lookup
test_codex_output_arguments
test_codex_generator_requires_an_installed_cli
test_codex_generator_cleans_up_in_a_standalone_process
test_contract_comparison
test_drift_report
test_claude_bare_reference
test_issue_deduplication
echo 'vendor drift script tests passed'
