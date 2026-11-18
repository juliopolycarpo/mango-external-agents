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
# shellcheck source=check-vendor-acp-docs.sh
source "$repo_root/scripts/check-vendor-acp-docs.sh"

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

assert_not_contains() {
  local path="$1"
  local unexpected="$2"
  local message="$3"
  if grep -Fq -- "$unexpected" "$path"; then
    fail "$message; unexpected $unexpected in $path: $(tr '\n' '|' < "$path")"
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
  local registry="$repo_root/scripts/vendor-cli-pinned-sha256sums.txt"
  local workflow="$repo_root/.github/workflows/vendor-drift.yml"
  local vendor
  local version
  local key
  local target
  local asset
  local digest

  python3 - "$registry" <<'PYREGISTRY'
import re
import sys
from pathlib import Path

seen = set()
for number, line in enumerate(Path(sys.argv[1]).read_text().splitlines(), 1):
    if not line.strip() or line.lstrip().startswith("#"):
        continue
    fields = line.split()
    assert len(fields) == 4, f"expected four registry fields at line {number}"
    vendor, version, asset, digest = fields
    assert vendor in {"claude", "codex", "opencode"}, f"unknown registry vendor at line {number}"
    assert re.fullmatch(r"[0-9a-f]{64}", digest), f"invalid pinned SHA-256 at line {number}"
    key = (vendor, version, asset)
    assert key not in seen, f"duplicate pinned checksum entry at line {number}"
    seen.add(key)
PYREGISTRY

  for vendor in claude codex opencode; do
    case "$vendor" in
      claude) key='CLAUDE_VERSION:' ;;
      codex) key='CODEX_VERSION:' ;;
      opencode) key='OPENCODE_VERSION:' ;;
    esac
    version=$(awk -v key="$key" '$1 == key { print $2 }' "$workflow")
    if [ -z "$version" ]; then
      fail "expected current $vendor pin in $workflow, received none"
    fi
    for target in linux-x64 linux-arm64 darwin-x64 darwin-arm64 windows-x64 windows-arm64; do
      asset=$(asset_name "$vendor" "$target")
      digest=$(pinned_archive_digest "$vendor" "$version" "$asset")
      assert_eq "${#digest}" '64' "pinned digest length for $vendor $version $target"
    done
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
  local empty_bin="$test_root/no-codex-bin"
  # An empty directory rather than the system ones: a machine that happens to carry /usr/bin/codex
  # would run the real generator and pass this test for the wrong reason. `vendor_dir` is set here
  # too, so the refusal cannot depend on whatever the previous test left in the global.
  mkdir -p "$empty_bin"
  local vendor_dir="$test_root/unused-vendor-dir"
  PATH="$empty_bin"
  expect_status 2 generate_inventory
  PATH="$original_path"
  if [ -e "$vendor_dir" ]; then
    fail 'a refused Codex generator must not create its output directory'
  fi
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
  # This named fake copies the current inventory; its stamp need not match the fake CLI.
  cmp "$FAKE_CODEX_SCHEMA" "$output/schema.json" || fail 'standalone Codex inventory output'
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
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  printf '%s\n' 'Without it, `claude -p` discovers and loads them as an interactive session would. --bare will become the default for -p in a future release.' > "$reference"
  validate_reference "$reference" >/dev/null
  printf '%s\n' 'Without it, <code>claude&nbsp;-p</code>' 'discovers and loads them as an interactive session would.' > "$reference"
  validate_reference "$reference" >/dev/null
  printf '%s\n' 'bare mode is now the default.' > "$reference"
  expect_status 1 validate_reference "$reference"
  printf '%s\n' 'Without it, <code>claude -p</code> loads the same context as an interactive session.' 'Bare mode is now the default.' > "$reference"
  expect_status 1 validate_reference "$reference"
  printf '%s\n' '<script>Without it, <code>claude -p</code> loads the same context.</script>' '<style>Without it, claude -p discovers and loads them as an interactive session would.</style>' > "$reference"
  expect_status 1 validate_reference "$reference"
  printf '%s\n' '<pre>Without it, <code>claude -p</code> loads the same context.</pre>' > "$reference"
  expect_status 1 validate_reference "$reference"
  printf '%s\n' '```text' 'Without it, <code>claude -p</code> loads the same context.' '```' > "$reference"
  expect_status 1 validate_reference "$reference"
  printf '%s\n' 'The opt-in guarantee has unknown replacement wording.' > "$reference"
  expect_status 1 validate_reference "$reference"
  expect_status 2 validate_reference "$reference/missing"
}

# Writes the page the named fake fetcher serves for a URL, one argument per line; see
# test-fixtures/fake-doc-curl.sh.
write_fake_doc() {
  local root="$1"
  local url="$2"
  shift 2
  mkdir -p "$root"
  printf '%s\n' "$@" > "$root/$(printf '%s' "$url" | tr -c 'A-Za-z0-9' '_')"
}

# Serves a clean page for every document a vendor's watch reads.
write_clean_fake_docs() {
  local root="$1"
  rm -rf -- "$root"
  write_fake_doc "$root" 'https://code.claude.com/docs/llms.txt' '# Claude Code Docs'
  write_fake_doc "$root" 'https://code.claude.com/docs/en/cli-reference.md' 'claude -p --print "query"'
  write_fake_doc "$root" 'https://learn.chatgpt.com/llms.txt' '- [Codex CLI](https://learn.chatgpt.com/docs/codex/cli.md)'
  write_fake_doc "$root" 'https://learn.chatgpt.com/docs/developer-commands.md?surface=cli' 'codex exec "task"'
}

test_vendor_acp_document() {
  local document="$test_root/vendor-acp-document.md"
  local error="$test_root/vendor-acp-document.error"
  local url='https://example.test/reference.md'
  local mention
  printf '%s\n' '# Commands' 'codex exec runs a task. The macpherson and acpi names are unrelated.' > "$document"
  validate_document "$url" 'codex exec' "$document" >/dev/null
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  for mention in '| `codex acp` | Run as an ACP agent |' 'Use --acp to serve stdio.' 'Agent Client Protocol support' \
    'agent-client-protocol' 'See [claude-agent-acp](https://example.test/claude-agent-acp).' 'AcP'; do
    printf '%s\n' '# Commands' 'codex exec runs a task.' "$mention" > "$document"
    expect_status 1 validate_document "$url" 'codex exec' "$document"
  done
  validate_document "$url" 'codex exec' "$document" > /dev/null 2> "$error" || true
  assert_contains "$error" "expected no ACP page or command in $url" 'ACP mention names the document'
  assert_contains "$error" '3:AcP' 'ACP mention names the matching line'
  printf '%s\n' 'A page that moved and no longer lists the command.' > "$document"
  validate_document "$url" 'codex exec' "$document" > /dev/null 2> "$error" || true
  assert_contains "$error" "expected $url to contain codex exec" 'a moved page names the missing anchor'
  expect_status 2 validate_document "$url" 'codex exec' "$document"
  expect_status 2 validate_document "$url" 'codex exec' "$document.missing"
}

test_vendor_acp_docs_watch() {
  local docs="$test_root/vendor-acp-docs"
  local fake_bin="$test_root/fake-doc-bin"
  local error="$test_root/vendor-acp-docs.error"
  local allowlist="$test_root/acp-watch-scratch-allowlist.txt"
  local original_path="$PATH"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  local claude_index='https://code.claude.com/docs/llms.txt'
  local claude_reference='https://code.claude.com/docs/en/cli-reference.md'
  local codex_index='https://learn.chatgpt.com/llms.txt'
  local codex_reference='https://learn.chatgpt.com/docs/developer-commands.md?surface=cli'
  mkdir -p "$fake_bin"
  ln -sf "$repo_root/scripts/test-fixtures/fake-doc-curl.sh" "$fake_bin/curl"
  export PATH="$fake_bin:$PATH"
  export FAKE_DOC_ROOT="$docs"
  # An empty scratch allowlist: these cases are about the pages, not the committed entries.
  ACP_DOCS_ALLOWLIST="$allowlist"
  write_acp_allowlist "$allowlist"

  assert_eq "$(vendor_documents claude | wc -l | tr -d ' ')" '2' 'Claude watches its index and CLI reference'
  assert_eq "$(vendor_documents codex | wc -l | tr -d ' ')" '2' 'Codex watches its index and CLI reference'
  expect_status 2 vendor_documents gemini

  write_clean_fake_docs "$docs"
  check_vendor claude >/dev/null
  check_vendor codex >/dev/null
  expect_status 2 check_vendor gemini

  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' '- [ACP](https://code.claude.com/docs/en/acp.md): Run Claude Code as an ACP agent'
  expect_status 1 check_vendor claude
  check_vendor claude > /dev/null 2> "$error" || true
  assert_contains "$error" "expected no ACP page or command in $claude_index" 'the Claude index mention is named'
  check_vendor codex >/dev/null

  write_clean_fake_docs "$docs"
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  write_fake_doc "$docs" "$claude_reference" 'claude -p --print' '| `--acp` | Serve over ACP |'
  expect_status 1 check_vendor claude

  write_clean_fake_docs "$docs"
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  write_fake_doc "$docs" "$codex_reference" 'codex exec' '| `codex acp` | Run as an Agent Client Protocol agent |'
  expect_status 1 check_vendor codex
  check_vendor claude >/dev/null

  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" "$codex_index" '- [Codex CLI](https://learn.chatgpt.com/docs/codex/cli.md)' '- [ACP](https://learn.chatgpt.com/docs/acp.md)'
  expect_status 1 check_vendor codex

  # An unreadable page is "could not verify", and it outranks a mention found on the other page.
  rm -f -- "$docs/$(printf '%s' "$codex_reference" | tr -c 'A-Za-z0-9' '_')"
  expect_status 2 check_vendor codex
  check_vendor codex > /dev/null 2> "$error" || true
  assert_contains "$error" "expected to read $codex_reference, received a failed download" 'a failed download names the URL'

  # A page that answers but lost its anchor (a moved page) must not pass as clean.
  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" "$claude_reference" 'Page not found'
  expect_status 2 check_vendor claude

  expect_status 2 main
  expect_status 2 main claude "$test_root/unused-report.md" codex
  unset FAKE_DOC_ROOT
  ACP_DOCS_ALLOWLIST="$original_allowlist"
  export PATH="$original_path"
}

# Writes a reviewed allowlist from vendor, line and reason arguments, three per entry. The file
# separates fields with a TAB because a Markdown table row, the likeliest accepted line, carries
# `|` itself.
write_acp_allowlist() {
  local path="$1"
  shift
  printf '%s\n' '# test allowlist' '' > "$path"
  while [ $# -gt 0 ]; do
    printf '%s\t%s\t%s\n' "$1" "$2" "$3" >> "$path"
    shift 3
  done
}

test_vendor_acp_docs_report() {
  local docs="$test_root/vendor-acp-report-docs"
  local fake_bin="$test_root/fake-report-doc-bin"
  local allowlist="$test_root/acp-report-allowlist.txt"
  local report="$test_root/acp-report/acp-docs.md"
  local expected="$test_root/acp-report-expected.md"
  local original_path="$PATH"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  local claude_index='https://code.claude.com/docs/llms.txt'
  local claude_reference='https://code.claude.com/docs/en/cli-reference.md'
  local reviewed='- [claude-agent-acp](https://code.claude.com/docs/en/adapter.md): Third-party ACP adapter'
  mkdir -p "$fake_bin"
  ln -sf "$repo_root/scripts/test-fixtures/fake-doc-curl.sh" "$fake_bin/curl"
  export PATH="$fake_bin:$PATH"
  export FAKE_DOC_ROOT="$docs"
  ACP_DOCS_ALLOWLIST="$allowlist"
  write_acp_allowlist "$allowlist"

  # A passing watch writes no report and removes one left by an earlier run.
  write_clean_fake_docs "$docs"
  mkdir -p "$(dirname "$report")"
  printf 'left over from an earlier run\n' > "$report"
  check_vendor claude "$report" > /dev/null
  if [ -e "$report" ]; then
    fail "a passing watch must not leave a report; received $(tr '\n' '|' < "$report")"
  fi

  # A mention reports the page URL and the matching line with its line number. Exit status stays 1.
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' '- [ACP](https://code.claude.com/docs/en/acp.md): Run Claude Code as an ACP agent'
  expect_status 1 check_vendor claude "$report"
  cat > "$expected" <<'EXPECTED'
### https://code.claude.com/docs/llms.txt

Lines naming ACP that no reviewed allowlist entry accepts, with their line numbers (showing 1 of 1; each line is cut at 1000 bytes):

```text
2:- [ACP](https://code.claude.com/docs/en/acp.md): Run Claude Code as an ACP agent
```

EXPECTED
  assert_eq "$(cat "$report")" "$(cat "$expected")" 'ACP mention report shape'
  # The fake curl is silent on success and the report holds page text only: nothing else leaks in.
  assert_not_contains "$report" "$test_root" 'the report names no local path'

  # Both pages are reported in one report, and a reviewed line is left out of it.
  write_acp_allowlist "$allowlist" claude "$reviewed" 'third-party adapter'
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$reviewed" '- [ACP](https://code.claude.com/docs/en/acp.md)'
  # shellcheck disable=SC2016 # the backticks are literal Markdown in the fake page, not command substitution
  write_fake_doc "$docs" "$claude_reference" 'claude -p --print' '| `--acp` | Serve over ACP |'
  expect_status 1 check_vendor claude "$report"
  assert_contains "$report" '### https://code.claude.com/docs/llms.txt' 'the index page is reported'
  assert_contains "$report" '3:- [ACP](https://code.claude.com/docs/en/acp.md)' 'the index line is reported'
  assert_contains "$report" '### https://code.claude.com/docs/en/cli-reference.md' 'the CLI reference page is reported'
  # shellcheck disable=SC2016 # the backticks are literal Markdown in the expected report line, not command substitution
  assert_contains "$report" '2:| `--acp` | Serve over ACP |' 'the CLI reference line is reported'
  assert_not_contains "$report" 'claude-agent-acp' 'a reviewed line is not reported'

  # More mentions than the report shows are counted, so a maintainer knows to open the page.
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' 'ACP one' 'ACP two' 'ACP three' 'ACP four'
  expect_status 1 check_vendor claude "$report"
  assert_contains "$report" '(showing 3 of 4; each line is cut at 1000 bytes)' 'a long list states how much it shows'
  assert_not_contains "$report" 'ACP four' 'the fourth mention stays out of the report'

  # An unreadable page reports its URL and no curl output; exit status 2.
  write_clean_fake_docs "$docs"
  write_acp_allowlist "$allowlist"
  rm -f -- "$docs/$(printf '%s' "$claude_reference" | tr -c 'A-Za-z0-9' '_')"
  expect_status 2 check_vendor claude "$report"
  assert_contains "$report" "### $claude_reference" 'a failed download names the URL'
  assert_contains "$report" 'The page could not be downloaded.' 'a failed download says so'
  assert_not_contains "$report" 'fake curl' 'curl output stays out of the report'

  # A moved page reports the text it lost; exit status 2.
  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" "$claude_reference" 'Page not found'
  expect_status 2 check_vendor claude "$report"
  assert_contains "$report" "### $claude_reference" 'a moved page names the URL'
  # shellcheck disable=SC2016 # the backticks are literal Markdown in the expected report line, not command substitution
  assert_contains "$report" 'The page no longer contains the text `--print`' 'a moved page names the lost anchor'

  # A stale allowlist entry is reported by its line, without the allowlist path; exit status 1.
  write_acp_allowlist "$allowlist" claude "$reviewed" 'third-party adapter'
  write_clean_fake_docs "$docs"
  expect_status 1 check_vendor claude "$report"
  assert_contains "$report" '### Reviewed allowlist' 'a stale entry has its own section'
  assert_contains "$report" "1:$reviewed" 'a stale entry names its line, numbered'
  assert_not_contains "$report" "$allowlist" 'the report names no local allowlist path'

  # The report is optional, `main` passes it through and refuses a third argument.
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' 'Run with --acp.'
  write_acp_allowlist "$allowlist"
  rm -f -- "$report"
  expect_status 1 main claude "$report"
  assert_contains "$report" '2:Run with --acp.' 'main writes the report'
  expect_status 1 main claude
  expect_status 2 main claude "$report" extra

  unset FAKE_DOC_ROOT
  ACP_DOCS_ALLOWLIST="$original_allowlist"
  export PATH="$original_path"
}

# An allowlist entry is a whole page line, so the report must carry a long llms.txt index line whole
# and still fit the 65536 bytes the "Bound the drift result" step allows.
test_vendor_acp_docs_report_long_lines() {
  local docs="$test_root/vendor-acp-long-docs"
  local fake_bin="$test_root/fake-long-doc-bin"
  local allowlist="$test_root/acp-long-allowlist.txt"
  local report="$test_root/acp-long-report/acp-docs.md"
  local combined="$test_root/acp-long-report/report.md"
  local committed="$test_root/acp-long-committed"
  local observed="$test_root/acp-long-observed"
  local original_path="$PATH"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  local claude_index='https://code.claude.com/docs/llms.txt'
  local claude_reference='https://code.claude.com/docs/en/cli-reference.md'
  local long_line
  local overlong_line
  local multibyte_line
  local reported
  local reported_bytes
  local character
  local padding
  local cut_input
  local cut_output
  local bytes
  mkdir -p "$fake_bin"
  ln -sf "$repo_root/scripts/test-fixtures/fake-doc-curl.sh" "$fake_bin/curl"
  export PATH="$fake_bin:$PATH"
  export FAKE_DOC_ROOT="$docs"
  ACP_DOCS_ALLOWLIST="$allowlist"
  write_acp_allowlist "$allowlist"

  # A line well past the old 200-character cut is reported whole.
  long_line="- [Reference](https://code.claude.com/docs/en/reference.md): ACP $(printf 'detail %.0s' {1..100})"
  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$long_line"
  expect_status 1 check_vendor claude "$report"
  assert_contains "$report" "2:$long_line" 'a long matching line is reported whole'

  # A line past the bound is cut at it, not dropped: the report names the bound.
  overlong_line="ACP $(head -c 5000 /dev/zero | tr '\0' 'x')"
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$overlong_line"
  expect_status 1 check_vendor claude "$report"
  reported=$(grep -m 1 '^2:ACP ' "$report")
  assert_eq "${#reported}" "$ACP_LINE_CUT" 'an overlong matching line is cut at the bound'
  assert_contains "$report" "each line is cut at $ACP_LINE_CUT bytes" 'the report names the cut'

  # The cut counts bytes, so it must not leave half of a multibyte character in the report: "2:ACP x"
  # is 7 bytes, so the 1000-byte cut falls between the two bytes of the 497th two-byte character.
  multibyte_line="ACP x$(printf 'é%.0s' {1..600})"
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$multibyte_line"
  expect_status 1 check_vendor claude "$report"
  if ! python3 -I -c 'import sys; sys.stdin.buffer.read().decode("utf-8")' < "$report" 2> "$test_root/acp-long-utf8.error"; then
    fail "(report encoding): expected valid UTF-8 after a cut inside a multibyte character | received $(tr '\n' '|' < "$test_root/acp-long-utf8.error")"
  fi
  reported_bytes=$(grep -m 1 '^2:ACP x' "$report" | wc -c | tr -d ' ')
  # wc counts the newline too.
  if [ "$reported_bytes" -gt "$((ACP_LINE_CUT + 1))" ]; then
    fail "(report line length): expected at most $ACP_LINE_CUT bytes | received $((reported_bytes - 1))"
  fi

  # Every split of a two-, three- and four-byte character leaves valid UTF-8, and a character the
  # cut leaves whole stays.
  for character in 'é' '€' '😀'; do
    for padding in 995 996 997 998 999 1000; do
      cut_input="$(head -c "$padding" /dev/zero | tr '\0' 'x')${character}tail"
      cut_output=$(printf '%s\n' "$cut_input" | cut_lines_to_report_bytes)
      if ! printf '%s' "$cut_output" | python3 -I -c 'import sys; sys.stdin.buffer.read().decode("utf-8")' 2> /dev/null; then
        fail "(cut_lines_to_report_bytes): expected valid UTF-8 for $padding bytes then $character | received invalid bytes"
      fi
    done
  done
  assert_eq "$(printf '%s\n' "$(head -c 996 /dev/zero | tr '\0' 'x')é" | cut_lines_to_report_bytes | tail -c 4)" 'xé' \
    'a two-byte character that fits before the cut is kept'

  # The worst docs section on both pages of a vendor, joined to a diff of the largest size, still
  # fits the bound the workflow enforces.
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$overlong_line" "$overlong_line" "$overlong_line" "$overlong_line"
  write_fake_doc "$docs" "$claude_reference" 'claude -p --print' "$overlong_line" "$overlong_line" "$overlong_line" "$overlong_line"
  expect_status 1 check_vendor claude "$report"
  mkdir -p "$committed" "$observed"
  seq 1 40000 > "$committed/contract.json"
  seq 40001 80000 > "$observed/contract.json"
  write_report claude "$committed" "$observed" "$combined" || [ $? -eq 1 ]
  write_docs_report claude "$report" "$combined"
  bytes=$(wc -c < "$combined")
  if [ "$bytes" -gt 65536 ]; then
    fail "(drift report size): expected at most 65536 bytes with a full diff and a full docs section | received $bytes"
  fi

  unset FAKE_DOC_ROOT
  ACP_DOCS_ALLOWLIST="$original_allowlist"
  export PATH="$original_path"
}

# The watch tests must not read the committed allowlist: its first real entry would match no line of
# their fake pages and fail every one of them. Stand in for that entry by pointing the inherited
# allowlist at a populated file, then run each watch test against it.
test_vendor_acp_docs_tests_ignore_the_inherited_allowlist() {
  local committed_stand_in="$test_root/acp-committed-stand-in.txt"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  write_acp_allowlist "$committed_stand_in" claude '- [x-acp](https://e.test/a.md): adapter' 'third-party'
  ACP_DOCS_ALLOWLIST="$committed_stand_in"
  test_vendor_acp_docs_watch
  assert_eq "$ACP_DOCS_ALLOWLIST" "$committed_stand_in" 'the watch test restores the allowlist it found'
  test_vendor_acp_docs_report
  assert_eq "$ACP_DOCS_ALLOWLIST" "$committed_stand_in" 'the report test restores the allowlist it found'
  ACP_DOCS_ALLOWLIST="$original_allowlist"
}

test_docs_drift_report() {
  local docs_report="$test_root/docs-report-fragment.md"
  local report="$test_root/docs-report/report.md"
  local expected="$test_root/docs-report-expected.md"
  local committed="$test_root/docs-report-committed"
  local observed="$test_root/docs-report-observed"
  # shellcheck disable=SC2016 # the backticks are literal Markdown in the fake report, not command substitution
  printf '### https://example.test/reference.md\n\nLines naming ACP:\n\n```text\n3:Use --acp.\n```\n\n' > "$docs_report"

  # With no contract difference the docs result is the whole report, led by the managed marker.
  rm -rf -- "$(dirname "$report")"
  write_docs_report codex "$docs_report" "$report"
  cat > "$expected" <<'EXPECTED'
<!-- vendor-drift:codex -->
# Vendor drift: codex

## Documentation watch

The documentation watch for an ACP page or command failed in this run: a vendor document
now mentions ACP outside the reviewed allowlist, or could not be read.

### https://example.test/reference.md

Lines naming ACP:

```text
3:Use --acp.
```

EXPECTED
  assert_eq "$(cat "$report")" "$(cat "$expected")" 'docs-only drift report shape'
  assert_eq "$(head -n 1 "$report")" '<!-- vendor-drift:codex -->' 'the marker leads the docs-only report'

  # With a contract difference the diff stays and the docs section follows it.
  mkdir -p "$committed" "$observed"
  printf 'same\n' > "$committed/contract.json"
  printf 'different\n' > "$observed/contract.json"
  expect_status 1 write_report claude "$committed" "$observed" "$report"
  write_docs_report claude "$docs_report" "$report"
  assert_eq "$(head -n 1 "$report")" '<!-- vendor-drift:claude -->' 'the marker leads the combined report'
  assert_contains "$report" 'diff --git' 'the contract diff is kept'
  assert_contains "$report" '## Documentation watch' 'the docs section is added'
  assert_contains "$report" '3:Use --acp.' 'the matching line is added'
  assert_eq "$(grep -c 'vendor-drift:' "$report")" '1' 'the marker appears once'

  # A watch that failed before it wrote page detail says so instead of an empty section.
  rm -f -- "$report"
  write_docs_report claude "$docs_report.missing" "$report"
  assert_contains "$report" 'The watch wrote no page detail. Inspect this workflow run for its output.' \
    'a missing docs report is explained'

  # The command line mode needs its three arguments. The script runs as a process because every
  # sourced script defines its own `main`.
  expect_status 2 "$repo_root/scripts/write-vendor-drift-report.sh" --docs claude "$docs_report"
  rm -f -- "$report"
  "$repo_root/scripts/write-vendor-drift-report.sh" --docs claude "$docs_report" "$report"
  assert_contains "$report" '<!-- vendor-drift:claude -->' 'the --docs mode writes the report'
}

# Writes the help of one command of the named fake vendor CLI: the file the fake serves for
# `<command> --help`. Each further argument is one line.
write_fake_subcommand_help() {
  local root="$1"
  local command="$2"
  shift 2
  mkdir -p "$root"
  printf '%s\n' "$@" > "$root/$command.help"
}

# A top-level help in the two shapes the CLIs print: Claude's commander layout, with aliases, an
# argument placeholder and wrapped descriptions, and Codex's clap layout, with a `help` entry and
# an option list after the commands.
write_fake_claude_top_level_help() {
  printf '%s\n' \
    'Usage: claude [options] [command] [prompt]' \
    '' \
    'Options:' \
    '  --print  Print and exit' \
    '' \
    'Commands:' \
    '  attach <id|name>                      Open a background session in this' \
    '                                        terminal. Part of the session name' \
    '                                        works too' \
    '  auth                                  Manage authentication' \
    '  mcp                                   Configure and manage MCP servers' \
    '  plugin|plugins                        Manage Claude Code plugins' \
    '  update|upgrade                        Check for updates' > "$1"
}

write_fake_codex_top_level_help() {
  # shellcheck disable=SC2016 # the backticks are literal help text, not command substitution
  printf '%s\n' \
    'Codex CLI' \
    '' \
    'Commands:' \
    '  exec              Run Codex non-interactively [aliases: e]' \
    '  mcp               Manage external MCP servers for Codex' \
    '  apply             Apply the latest diff produced by Codex agent as a `git apply` to your local' \
    '                    working tree [aliases: a]' \
    '  help              Print this message or the help of the given subcommand(s)' \
    '' \
    'Options:' \
    '  -h, --help  Print help' > "$1"
}

test_vendor_subcommand_help_watch() {
  local watch="$repo_root/scripts/check-vendor-subcommand-help.sh"
  local fake_cli="$repo_root/scripts/test-fixtures/fake-vendor-cli.sh"
  local root="$test_root/subcommand-help-root"
  local log="$test_root/subcommand-help.log"
  local top="$test_root/subcommand-top-help.txt"
  local report="$test_root/subcommand-help-report/subcommand-help.md"
  local error="$test_root/subcommand-help.error"
  local command
  export VENDOR_HELP_CLI="$fake_cli"
  export FAKE_VENDOR_CLI_ROOT="$root"
  export FAKE_VENDOR_CLI_LOG="$log"

  # A clean Claude surface passes: every listed command is asked for `--help` once and for nothing
  # else, the alias after `|` and the wrapped description lines are not commands, and a passing
  # watch writes no report and removes one left by an earlier run. The macpherson and acpi words
  # are not ACP.
  write_fake_claude_top_level_help "$top"
  rm -rf -- "$root"
  for command in attach auth mcp plugin update; do
    write_fake_subcommand_help "$root" "$command" "Usage: claude $command" 'Options:' '  --verbose  The macpherson and acpi names are unrelated.'
  done
  : > "$log"
  mkdir -p "$(dirname "$report")"
  printf 'left over from an earlier run\n' > "$report"
  "$watch" claude "$top" "$report" > /dev/null 2> "$error" || fail "a clean Claude surface must pass; received $(cat "$error")"
  assert_eq "$(sed 's/ HOME=.*//' "$log" | sort | tr '\n' ',')" \
    'argv=attach --help,argv=auth --help,argv=mcp --help,argv=plugin --help,argv=update --help,' \
    'the watch asks each listed command for --help and nothing else'
  if [ -e "$report" ]; then
    fail "a passing watch must not leave a report; received $(tr '\n' '|' < "$report")"
  fi

  # The CLI runs under a scratch home, never the caller's own.
  assert_not_contains "$log" "HOME=$HOME " 'the CLI must not run under the caller home'
  assert_not_contains "$log" 'HOME= ' 'the CLI runs under a home'
  assert_contains "$log" 'CLAUDE_CONFIG_DIR=' 'the CLI runs under a scratch Claude configuration'
  assert_not_contains "$log" 'CLAUDE_CONFIG_DIR= ' 'the Claude configuration directory is set'
  if grep -Eq 'CODEX_HOME=$' "$log"; then
    fail "the Codex home must be set to a scratch directory; received $(tr '\n' '|' < "$log")"
  fi

  # A flag under one subcommand is a failed watch (1) that names the command and the line with its
  # number, in the console and in the report, and leaves the other commands out of the report.
  write_fake_subcommand_help "$root" mcp 'Usage: claude mcp' 'Options:' '  --acp  Serve the Agent Client Protocol over stdio'
  expect_status 1 "$watch" claude "$top" "$report"
  "$watch" claude "$top" "$report" > /dev/null 2> "$error" || true
  assert_contains "$error" 'expected no ACP flag or command in claude mcp --help, received:' 'the console names the command'
  assert_contains "$error" '3:  --acp  Serve the Agent Client Protocol over stdio' 'the console names the line with its number'
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_contains "$report" '### `claude mcp --help`' 'the report names the command line'
  assert_contains "$report" '3:  --acp  Serve the Agent Client Protocol over stdio' 'the report holds the matching line'
  assert_not_contains "$report" 'claude auth' 'a clean command is not in the report'
  assert_not_contains "$report" 'Usage: claude mcp' 'only the matching line of the help is reported'

  # A word match, not a substring: "Agent Client Protocols" and "--acp" match, "macpherson" does not.
  write_fake_subcommand_help "$root" mcp 'agent-client-protocol bridge'
  expect_status 1 "$watch" claude "$top" "$report"
  write_fake_subcommand_help "$root" mcp 'The macpherson and acpi names are unrelated.'
  expect_status 0 "$watch" claude "$top" "$report"

  # A command whose help cannot be read is "could not verify" (2), and outranks a mention.
  write_fake_subcommand_help "$root" auth '  --acp  Serve ACP'
  rm -f -- "$root/plugin.help"
  expect_status 2 "$watch" claude "$top" "$report"
  "$watch" claude "$top" "$report" > /dev/null 2> "$error" || true
  assert_contains "$error" 'expected claude plugin --help to exit 0, received exit status 2' 'the unreadable command is named'
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_contains "$report" '### `claude plugin --help`' 'the report names the unreadable command'
  assert_contains "$report" 'so its help was not searched' 'the report says the help was not searched'
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_contains "$report" '### `claude auth --help`' 'the mention is reported beside the unreadable command'
  write_fake_subcommand_help "$root" plugin 'Usage: claude plugin'

  # A CLI that cannot be started is also unverified.
  VENDOR_HELP_CLI="$test_root/no-such-vendor-cli" expect_status 2 "$watch" claude "$top" "$report"

  # A command that never answers is cut off after the limit, named, and is "could not verify" (2),
  # not a hung lane. The fake CLI hangs for `mcp` when a .hang file stands beside its help pages.
  : > "$root/mcp.hang"
  local started=$SECONDS
  HELP_TIMEOUT_SECONDS=1 expect_status 2 "$watch" claude "$top" "$report"
  HELP_TIMEOUT_SECONDS=1 "$watch" claude "$top" "$report" > /dev/null 2> "$error" || true
  if [ $((SECONDS - started)) -gt 30 ]; then
    fail "a hung command must be cut off after HELP_TIMEOUT_SECONDS=1 | received $((SECONDS - started)) seconds for two runs"
  fi
  assert_contains "$error" 'expected claude mcp --help to answer within 1 seconds, received no answer' 'the hung command is named with the limit'
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_contains "$report" '### `claude mcp --help`' 'the report names the hung command'
  assert_contains "$report" 'did not answer within 1 seconds' 'the report says the command timed out'
  assert_not_contains "$error" 'claude auth --help to answer' 'only the hung command is reported'
  rm -f -- "$root/mcp.hang"
  # The default limit is long enough for a real CLI and a plain number of seconds.
  assert_contains "$watch" 'HELP_TIMEOUT_SECONDS:-30' 'the default limit is 30 seconds'
  HELP_TIMEOUT_SECONDS=soon expect_status 2 "$watch" claude "$top" "$report"

  # More flagged commands than the report shows are counted and named, so the report stays bounded.
  for command in attach auth mcp plugin update; do
    write_fake_subcommand_help "$root" "$command" '  --acp  Serve ACP'
  done
  printf '%s\n' 'Commands:' '  one  x' '  two  x' '  three  x' '  four  x' '  five  x' '  six  x' '  seven  x' '  eight  x' > "$top"
  for command in one two three four five six seven eight; do
    write_fake_subcommand_help "$root" "$command" '  --acp  Serve ACP'
  done
  expect_status 1 "$watch" claude "$top" "$report"
  assert_eq "$(grep -c '^### ' "$report")" '7' 'six commands and one note for the rest'
  assert_contains "$report" 'Also naming ACP, not shown here: seven eight.' 'the commands past the bound are named'

  # A long help line is cut at 240 bytes and a split UTF-8 character is dropped, as in the docs watch.
  write_fake_claude_top_level_help "$top"
  for command in attach auth plugin update; do
    write_fake_subcommand_help "$root" "$command" 'clean'
  done
  # The line is "1:ACP " plus 233 x, so byte 240 is the first byte of the two-byte é.
  write_fake_subcommand_help "$root" mcp "ACP $(printf 'x%.0s' $(seq 1 233))é$(printf 'y%.0s' $(seq 1 50))"
  expect_status 1 "$watch" claude "$top" "$report"
  assert_contains "$report" "ACP $(printf 'x%.0s' $(seq 1 233))" 'the reported help line keeps its first bytes'
  assert_not_contains "$report" 'yyy' 'a reported help line is cut at 240 bytes'
  if LC_ALL=C grep -q $'\xc3$' "$report"; then
    fail 'a cut must not leave half a UTF-8 character in the report'
  fi

  # Codex's layout: `help` is not probed, a wrapped description is not a command, and the same
  # fake serves the vendor name as the report heading.
  write_fake_codex_top_level_help "$top"
  rm -rf -- "$root"
  for command in exec mcp apply; do
    write_fake_subcommand_help "$root" "$command" "Usage: codex $command"
  done
  : > "$log"
  "$watch" codex "$top" "$report" > /dev/null
  assert_eq "$(sed 's/ HOME=.*//' "$log" | sort | tr '\n' ',')" 'argv=apply --help,argv=exec --help,argv=mcp --help,' \
    'the Codex commands are probed and help is not'
  write_fake_subcommand_help "$root" exec '      --acp  Run over ACP'
  expect_status 1 "$watch" codex "$top" "$report"
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_contains "$report" '### `codex exec --help`' 'the report names the Codex command line'

  # No command in the top-level help, an unreadable help file and an odd command name are all
  # "could not verify", and the watch runs no CLI for them.
  : > "$log"
  printf 'Usage: claude\n\nOptions:\n  --print\n' > "$top"
  expect_status 2 "$watch" claude "$top" "$report"
  assert_contains "$report" 'The top-level help lists no subcommand' 'a help with no commands is explained'
  expect_status 2 "$watch" claude "$top.missing" "$report"
  printf 'Commands:\n  mcp  x\n  --acp  x\n' > "$top"
  expect_status 2 "$watch" claude "$top" "$report"
  assert_eq "$(wc -c < "$log" | tr -d ' ')" '0' 'a top-level help that cannot be read runs no CLI'
  expect_status 2 "$watch" opencode "$top" "$report"
  expect_status 2 "$watch" claude
  unset VENDOR_HELP_CLI FAKE_VENDOR_CLI_ROOT FAKE_VENDOR_CLI_LOG
}

test_help_probe_drift_report() {
  local probe_report="$test_root/help-probe-fragment.md"
  local report="$test_root/help-probe-report/report.md"
  local expected="$test_root/help-probe-expected.md"
  # shellcheck disable=SC2016 # the backticks are literal Markdown in the fake report, not command substitution
  printf '### `claude mcp --help`\n\nLines naming ACP:\n\n```text\n3:  --acp  Serve ACP\n```\n\n' > "$probe_report"

  # With no other difference the probe result is the whole report, led by the managed marker.
  rm -rf -- "$(dirname "$report")"
  "$repo_root/scripts/write-vendor-drift-report.sh" --help-probe claude "$probe_report" "$report"
  cat > "$expected" <<'EXPECTED'
<!-- vendor-drift:claude -->
# Vendor drift: claude

## Subcommand help watch

The subcommand help watch for an ACP flag or command failed in this run: the help of a
subcommand now names ACP, or could not be read.

### `claude mcp --help`

Lines naming ACP:

```text
3:  --acp  Serve ACP
```

EXPECTED
  assert_eq "$(cat "$report")" "$(cat "$expected")" 'probe-only drift report shape'

  # Beside the documentation watch the marker stays once and both sections remain.
  printf '### https://example.test/reference.md\n\nLines naming ACP:\n' > "$test_root/help-probe-docs.md"
  "$repo_root/scripts/write-vendor-drift-report.sh" --docs claude "$test_root/help-probe-docs.md" "$report"
  assert_contains "$report" '## Documentation watch' 'the docs section is kept'
  assert_contains "$report" '## Subcommand help watch' 'the probe section follows the docs section'
  assert_eq "$(grep -c 'vendor-drift:' "$report")" '1' 'the marker appears once'

  # A probe that wrote no detail says so, and the mode needs its three arguments.
  rm -f -- "$report"
  "$repo_root/scripts/write-vendor-drift-report.sh" --help-probe claude "$probe_report.missing" "$report"
  assert_contains "$report" 'The watch wrote no command detail. Inspect this workflow run for its output.' 'a missing probe report is explained'
  assert_not_contains "$report" 'page detail' 'the help watch reads no pages'
  expect_status 2 "$repo_root/scripts/write-vendor-drift-report.sh" --help-probe claude "$probe_report"
}

test_vendor_acp_allowlist_entries() {
  local allowlist="$test_root/acp-allowlist.txt"
  local error="$test_root/acp-allowlist.error"
  local original="$ACP_DOCS_ALLOWLIST"
  ACP_DOCS_ALLOWLIST="$allowlist"
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  write_acp_allowlist "$allowlist" \
    claude '- [claude-agent-acp](https://example.test/adapter.md): adapter' 'third-party adapter index line' \
    codex '| `codex acp` | Run as an ACP agent |' 'reviewed table row'
  assert_eq "$(accepted_lines claude)" '- [claude-agent-acp](https://example.test/adapter.md): adapter' \
    'Claude accepted line'
  # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
  assert_eq "$(accepted_lines codex)" '| `codex acp` | Run as an ACP agent |' \
    'Codex accepted line keeps its own pipes'
  write_acp_allowlist "$allowlist"
  assert_eq "$(accepted_lines claude)" '' 'an allowlist without entries accepts nothing'

  printf 'claude\tan accepted line with no reason\n' >> "$allowlist"
  expect_status 2 accepted_lines claude
  accepted_lines claude > /dev/null 2> "$error" || true
  assert_contains "$error" 'received: claude	an accepted line with no reason' 'a missing reason names the entry'
  write_acp_allowlist "$allowlist"
  printf 'gemini\tsome line\tsome reason\n' >> "$allowlist"
  expect_status 2 accepted_lines claude
  ACP_DOCS_ALLOWLIST="$test_root/acp-allowlist.missing"
  expect_status 2 accepted_lines claude
  ACP_DOCS_ALLOWLIST="$original"
}

test_vendor_acp_document_allowlist() {
  local document="$test_root/vendor-acp-allowlisted.md"
  local accepted="$test_root/vendor-acp-accepted.txt"
  local seen="$test_root/vendor-acp-seen.txt"
  local error="$test_root/vendor-acp-allowlisted.error"
  local url='https://example.test/reference.md'
  local reviewed='- [claude-agent-acp](https://example.test/adapter.md): adapter'
  printf '%s\n' "$reviewed" > "$accepted"
  : > "$seen"
  printf '%s\n' '# Commands' 'codex exec runs a task.' "$reviewed" > "$document"
  validate_document "$url" 'codex exec' "$document" "$accepted" "$seen" > /dev/null
  assert_eq "$(cat "$seen")" "$reviewed" 'the accepted line is recorded as used'
  # The reviewed line passes; the new one is the only one reported.
  printf '%s\n' '# Commands' 'codex exec runs a task.' "$reviewed" 'New: run with --acp.' > "$document"
  expect_status 1 validate_document "$url" 'codex exec' "$document" "$accepted" "$seen"
  validate_document "$url" 'codex exec' "$document" "$accepted" "$seen" > /dev/null 2> "$error" || true
  assert_contains "$error" '4:New: run with --acp.' 'the new mention is named'
  assert_not_contains "$error" 'claude-agent-acp' 'the reviewed mention is not reported'
  # A reworded line is not the reviewed line.
  printf '%s\n' '# Commands' 'codex exec runs a task.' "$reviewed (updated)" > "$document"
  expect_status 1 validate_document "$url" 'codex exec' "$document" "$accepted" "$seen"
  # A partial match is not a match: the entry must equal the whole line.
  printf '%s\n' '# Commands' 'codex exec runs a task.' "prefix $reviewed" > "$document"
  expect_status 1 validate_document "$url" 'codex exec' "$document" "$accepted" "$seen"
  # A page that ends its lines with CRLF still matches the reviewed text.
  printf '%s\r\n' '# Commands' 'codex exec runs a task.' "$reviewed" > "$document"
  validate_document "$url" 'codex exec' "$document" "$accepted" "$seen" > /dev/null
}

test_vendor_acp_docs_allowlist_watch() {
  local docs="$test_root/vendor-acp-allowlist-docs"
  local fake_bin="$test_root/fake-allowlist-doc-bin"
  local allowlist="$test_root/acp-watch-allowlist.txt"
  local error="$test_root/vendor-acp-allowlist.error"
  local original_path="$PATH"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  local claude_index='https://code.claude.com/docs/llms.txt'
  local claude_reference='https://code.claude.com/docs/en/cli-reference.md'
  local reviewed='- [claude-agent-acp](https://code.claude.com/docs/en/adapter.md): Third-party ACP adapter'
  mkdir -p "$fake_bin"
  ln -s "$repo_root/scripts/test-fixtures/fake-doc-curl.sh" "$fake_bin/curl"
  export PATH="$fake_bin:$PATH"
  export FAKE_DOC_ROOT="$docs"
  ACP_DOCS_ALLOWLIST="$allowlist"
  write_acp_allowlist "$allowlist" claude "$reviewed" 'third-party adapter, not a Claude Code ACP mode'

  # The reviewed line passes.
  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$reviewed"
  check_vendor claude > /dev/null

  # A new mention beside it fails and is the only line reported.
  write_fake_doc "$docs" "$claude_index" '# Claude Code Docs' "$reviewed" '- [ACP](https://code.claude.com/docs/en/acp.md): Run as an ACP agent'
  expect_status 1 check_vendor claude
  check_vendor claude > /dev/null 2> "$error" || true
  assert_contains "$error" '- [ACP](https://code.claude.com/docs/en/acp.md)' 'the new mention is reported'
  assert_not_contains "$error" 'claude-agent-acp' 'the reviewed mention is not reported'

  # The entry is scoped to its vendor: Codex pages are not excused by a Claude entry.
  write_clean_fake_docs "$docs"
  write_fake_doc "$docs" 'https://learn.chatgpt.com/llms.txt' '- [Codex CLI](https://learn.chatgpt.com/docs/codex/cli.md)' "$reviewed"
  expect_status 1 check_vendor codex

  # A reviewed line that no page carries any more is stale. It fails and is named.
  write_clean_fake_docs "$docs"
  expect_status 1 check_vendor claude
  check_vendor claude > /dev/null 2> "$error" || true
  assert_contains "$error" "expected every claude entry in $allowlist to match a line in a watched page" \
    'a stale entry names the allowlist'
  assert_contains "$error" "received no match for:" 'a stale entry states what was received'
  assert_contains "$error" "$reviewed" 'a stale entry names its line'
  check_vendor codex > /dev/null

  # The entry matches on any watched page of its vendor, so the CLI reference can carry it too.
  write_fake_doc "$docs" "$claude_reference" 'claude -p --print' "$reviewed"
  check_vendor claude > /dev/null

  # An unreadable page could still carry the line: report "could not verify", not a stale entry.
  rm -f -- "$docs/$(printf '%s' "$claude_reference" | tr -c 'A-Za-z0-9' '_')"
  expect_status 2 check_vendor claude
  check_vendor claude > /dev/null 2> "$error" || true
  assert_not_contains "$error" 'expected every claude entry' 'an unreadable page hides no stale verdict'

  # A malformed allowlist refuses the run before any page is read.
  write_clean_fake_docs "$docs"
  printf 'claude\tno reason\n' >> "$allowlist"
  expect_status 2 check_vendor claude

  unset FAKE_DOC_ROOT
  ACP_DOCS_ALLOWLIST="$original_allowlist"
  export PATH="$original_path"
}

test_committed_acp_allowlist_is_well_formed() {
  accepted_lines claude > /dev/null
  accepted_lines codex > /dev/null
}

# Writes a scratch assessment whose "Accepted documentation mentions" section carries the given
# lines inside a text fence, after a decoy section whose fence always carries `a decoy line`.
write_acp_assessment() {
  local path="$1"
  shift
  {
    printf '%s\n' '# ACP convergence assessment' '' '## Another section' '' 'A decoy mention.' ''
    printf '%s\n' '```text' 'a decoy line' '```' ''
    printf '%s\n' '## Accepted documentation mentions' '' 'Prose before the entries.' '' '```text'
    printf '%s\n' "$@"
    printf '%s\n' '```' '' '## Closing section' '' '```text'
    printf '%s\n' 'a line after the section'
    printf '%s\n' '```'
  } > "$path"
}

test_vendor_acp_allowlist_entries_are_recorded() {
  local allowlist="$test_root/acp-record-allowlist.txt"
  local assessment="$test_root/acp-record-assessment.md"
  local error="$test_root/acp-record.error"
  local original_allowlist="$ACP_DOCS_ALLOWLIST"
  local original_assessment="$ACP_CONVERGENCE_DOC"
  local claude_row='- [claude-agent-acp](https://example.test/adapter.md): adapter | note'
  # The backticks are page text, not a command substitution.
  # shellcheck disable=SC2016
  local codex_row='| `codex acp` | Run as an ACP agent |'
  ACP_DOCS_ALLOWLIST="$allowlist"
  ACP_CONVERGENCE_DOC="$assessment"

  # Both entries present as whole lines of the section: recorded.
  write_acp_allowlist "$allowlist" claude "$claude_row" 'third-party adapter' codex "$codex_row" 'reviewed row'
  write_acp_assessment "$assessment" "$claude_row" "$codex_row"
  unrecorded_allowlist_entries

  # An allowlist without entries needs no record.
  write_acp_allowlist "$allowlist"
  write_acp_assessment "$assessment"
  unrecorded_allowlist_entries

  # One entry absent: it fails and names only that entry, with the allowlist and the section.
  write_acp_allowlist "$allowlist" claude "$claude_row" 'third-party adapter' codex "$codex_row" 'reviewed row'
  write_acp_assessment "$assessment" "$claude_row"
  expect_status 1 unrecorded_allowlist_entries
  unrecorded_allowlist_entries 2> "$error" || true
  assert_contains "$error" "expected every entry in $allowlist to be a whole line of the \"Accepted documentation mentions\" section of $assessment" \
    'an unrecorded entry names the allowlist, the section and the assessment'
  assert_contains "$error" "received no record for:" 'an unrecorded entry states what was received'
  assert_contains "$error" "$codex_row" 'the unrecorded entry is named'
  assert_not_contains "$error" 'claude-agent-acp' 'the recorded entry is not named'

  # A reworded or partial record is not a record.
  write_acp_assessment "$assessment" "$claude_row (updated)" "prefix $codex_row"
  expect_status 1 unrecorded_allowlist_entries

  # The entry text before the section does not count: only the decoy section carries it here.
  write_acp_allowlist "$allowlist" claude 'a decoy line' 'third-party adapter'
  write_acp_assessment "$assessment" 'another line'
  assert_contains "$assessment" 'a decoy line' 'the decoy section carries the entry'
  expect_status 1 unrecorded_allowlist_entries
  # The same entry inside the section is recorded.
  write_acp_assessment "$assessment" 'a decoy line'
  unrecorded_allowlist_entries
  # Nor does a line of a section after it.
  write_acp_allowlist "$allowlist" claude 'a line after the section' 'third-party adapter'
  expect_status 1 unrecorded_allowlist_entries

  # An assessment without the section records nothing.
  printf '%s\n' '# ACP convergence assessment' "$claude_row" > "$assessment"
  write_acp_allowlist "$allowlist" claude "$claude_row" 'third-party adapter'
  expect_status 1 unrecorded_allowlist_entries

  # A fenced page line that starts with `## ` does not end the section early.
  write_acp_assessment "$assessment" '## Heading-shaped page line' "$claude_row"
  write_acp_allowlist "$allowlist" claude "$claude_row" 'third-party adapter'
  unrecorded_allowlist_entries

  # A missing assessment and a malformed allowlist are errors, not an unrecorded entry.
  ACP_CONVERGENCE_DOC="$test_root/acp-record-assessment.missing"
  expect_status 2 unrecorded_allowlist_entries
  ACP_CONVERGENCE_DOC="$assessment"
  printf 'claude\tno reason\n' >> "$allowlist"
  expect_status 2 unrecorded_allowlist_entries

  ACP_DOCS_ALLOWLIST="$original_allowlist"
  ACP_CONVERGENCE_DOC="$original_assessment"
}

# The committed allowlist is recorded in the committed assessment.
test_committed_acp_allowlist_is_recorded() {
  unrecorded_allowlist_entries
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
  assert_contains "$log" 'issue edit 42 ' 'existing drift issue update'
  assert_contains "$log" 'labels=type:+drift' 'drift issues are read by label'
  assert_contains "$log" '--paginate' 'every page of drift issues is read'
  assert_not_contains "$log" 'issue create' 'a second run must not open a duplicate'
  assert_not_contains "$log" 'issue edit 1042' 'a pull request carrying the label is not a drift issue'

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
test_vendor_acp_document
test_vendor_acp_docs_watch
test_vendor_acp_docs_report
test_vendor_acp_docs_report_long_lines
test_vendor_acp_docs_tests_ignore_the_inherited_allowlist
test_docs_drift_report
test_vendor_subcommand_help_watch
test_help_probe_drift_report
test_vendor_acp_allowlist_entries
test_vendor_acp_document_allowlist
test_vendor_acp_docs_allowlist_watch
test_committed_acp_allowlist_is_well_formed
test_vendor_acp_allowlist_entries_are_recorded
test_committed_acp_allowlist_is_recorded
test_issue_deduplication
python3 scripts/test-vendor-result.py
python3 scripts/test-codex-schema-inventory.py
echo 'vendor drift script tests passed'
