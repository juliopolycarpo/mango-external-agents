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
  local original_path="$PATH"
  local claude_index='https://code.claude.com/docs/llms.txt'
  local claude_reference='https://code.claude.com/docs/en/cli-reference.md'
  local codex_index='https://learn.chatgpt.com/llms.txt'
  local codex_reference='https://learn.chatgpt.com/docs/developer-commands.md?surface=cli'
  mkdir -p "$fake_bin"
  ln -s "$repo_root/scripts/test-fixtures/fake-doc-curl.sh" "$fake_bin/curl"
  export PATH="$fake_bin:$PATH"
  export FAKE_DOC_ROOT="$docs"

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
  write_fake_doc "$docs" "$claude_reference" 'claude -p --print' '| `--acp` | Serve over ACP |'
  expect_status 1 check_vendor claude

  write_clean_fake_docs "$docs"
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
  expect_status 2 main claude codex
  unset FAKE_DOC_ROOT
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

test_vendor_acp_allowlist_entries() {
  local allowlist="$test_root/acp-allowlist.txt"
  local error="$test_root/acp-allowlist.error"
  local original="$ACP_DOCS_ALLOWLIST"
  ACP_DOCS_ALLOWLIST="$allowlist"
  write_acp_allowlist "$allowlist" \
    claude '- [claude-agent-acp](https://example.test/adapter.md): adapter' 'third-party adapter index line' \
    codex '| `codex acp` | Run as an ACP agent |' 'reviewed table row'
  assert_eq "$(accepted_lines claude)" '- [claude-agent-acp](https://example.test/adapter.md): adapter' \
    'Claude accepted line'
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
test_vendor_acp_allowlist_entries
test_vendor_acp_document_allowlist
test_vendor_acp_docs_allowlist_watch
test_committed_acp_allowlist_is_well_formed
test_issue_deduplication
python3 scripts/test-vendor-result.py
python3 scripts/test-codex-schema-inventory.py
echo 'vendor drift script tests passed'
