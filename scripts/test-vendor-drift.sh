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
  expect_status 2 vendor_repo unknown
  expect_status 2 asset_name claude solaris-sparc
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
test_codex_output_arguments
test_codex_generator_requires_an_installed_cli
test_contract_comparison
test_drift_report
test_claude_bare_reference
test_issue_deduplication
echo 'vendor drift script tests passed'
