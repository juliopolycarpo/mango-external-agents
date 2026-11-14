#!/usr/bin/env bash
# Watches a vendor's published documentation for an ACP page or command.
#
# The drift lane notices a native ACP mode through the captured `--help` text, which lags the
# vendor's documentation and sees only the top-level commands. This watch reads each vendor's
# documentation index and CLI reference (Markdown twins, public, no login) and fails when either
# names ACP. See docs/acp-convergence.md, "The drift-lane trigger".
#
# Exit status 0: no ACP mention. 1: a document mentions ACP. 2: a document could not be read or no
# longer looks like the page it was pinned to (a moved page would otherwise pass silently).
#
# Usage: scripts/check-vendor-acp-docs.sh <claude|codex>
set -euo pipefail

# Case-insensitive; `\b` keeps a hash or a longer word that merely contains "acp" from matching,
# while `--acp`, `acp` and `claude-agent-acp` do match.
readonly ACP_PATTERN='\bacp\b|agent[ -]?client[ -]?protocol'

usage() {
  echo 'usage: scripts/check-vendor-acp-docs.sh <claude|codex>' >&2
}

# Prints `url|anchor` per document. The anchor is text the page carries today; a page that stops
# carrying it was moved, emptied or replaced, and a clean ACP search of it proves nothing.
vendor_documents() {
  case "$1" in
    claude)
      echo 'https://code.claude.com/docs/llms.txt|Claude Code Docs'
      echo 'https://code.claude.com/docs/en/cli-reference.md|--print'
      ;;
    codex)
      echo 'https://learn.chatgpt.com/llms.txt|Codex CLI'
      echo 'https://learn.chatgpt.com/docs/developer-commands.md?surface=cli|codex exec'
      ;;
    *)
      printf 'expected vendor claude or codex, received %s\n' "$1" >&2
      return 2
      ;;
  esac
}

validate_document() {
  local url="$1"
  local anchor="$2"
  local document="$3"
  local matches
  if [ ! -f "$document" ]; then
    printf 'expected document file %s for %s, received none\n' "$document" "$url" >&2
    return 2
  fi
  if ! grep -Fq -- "$anchor" "$document"; then
    printf 'expected %s to contain %s, received no such text\n' "$url" "$anchor" >&2
    return 2
  fi
  matches=$(grep -nEi -- "$ACP_PATTERN" "$document" | head -n 3 | cut -c 1-200 || true)
  if [ -n "$matches" ]; then
    printf 'expected no ACP page or command in %s, received:\n%s\n' "$url" "$matches" >&2
    return 1
  fi
  printf 'no ACP mention in %s\n' "$url"
}

fetch_and_validate_document() {
  local url="$1"
  local anchor="$2"
  local document
  local status
  document=$(mktemp "${TMPDIR:-/tmp}/mea-vendor-acp-doc.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -f -- $(printf '%q' "$document")" RETURN
  if ! curl --fail --silent --show-error --location "$url" --output "$document"; then
    printf 'expected to read %s, received a failed download\n' "$url" >&2
    return 2
  fi
  if validate_document "$url" "$anchor" "$document"; then
    status=0
  else
    status=$?
  fi
  trap - RETURN
  rm -f -- "$document"
  return "$status"
}

# Reads every document before it answers, so one run lists each page that needs triage. An
# unreadable page (2) outranks a mention (1): "could not verify" is the stronger statement.
check_vendor() {
  local vendor="$1"
  local documents
  local url
  local anchor
  local status
  local worst=0
  documents=$(vendor_documents "$vendor") || return $?
  while IFS='|' read -r url anchor; do
    if fetch_and_validate_document "$url" "$anchor"; then
      status=0
    else
      status=$?
    fi
    if [ "$status" -eq 2 ] || { [ "$status" -eq 1 ] && [ "$worst" -eq 0 ]; }; then
      worst=$status
    fi
  done <<< "$documents"
  return "$worst"
}

main() {
  if [ $# -ne 1 ]; then
    usage
    return 2
  fi
  check_vendor "$1"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
