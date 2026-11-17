#!/usr/bin/env bash
# Watches a vendor's published documentation for an ACP page or command.
#
# The drift lane notices a native ACP mode through the captured `--help` text, which lags the
# vendor's documentation and sees only the top-level commands. This watch reads each vendor's
# documentation index and CLI reference (Markdown twins, public, no login) and fails when either
# names ACP. See docs/acp-convergence.md, "The drift-lane trigger".
#
# A mention the project decided to keep is recorded in scripts/vendor-acp-docs-allowlist.txt, one
# reviewed line per entry with the reason. The watch subtracts those lines before it fails, and it
# fails on an entry that no longer matches so a stale decision cannot excuse a later mention.
#
# With a report file, the watch also writes what a maintainer needs to triage without opening the
# run log: each page URL that needs attention and its matching line text (public page text and
# public URLs only, never curl output). The drift workflow appends it to the managed issue body;
# the file does not exist when the watch passes.
#
# Exit status 0: no unreviewed ACP mention. 1: a document mentions ACP outside the allowlist, or an
# allowlist entry matches no line. 2: a document could not be read or no longer looks like the page
# it was pinned to (a moved page would otherwise pass silently), or the allowlist is malformed.
#
# Usage: scripts/check-vendor-acp-docs.sh <claude|codex> [report-file]
set -euo pipefail

# Case-insensitive; `\b` keeps a hash or a longer word that merely contains "acp" from matching,
# while `--acp`, `acp` and `claude-agent-acp` do match.
readonly ACP_PATTERN='\bacp\b|agent[ -]?client[ -]?protocol'

# The most bytes of one reported page line (its line number included). An allowlist entry is a
# whole page line, so this is long enough for an llms.txt index line; it stays a bound because the
# report shares the 65536 bytes the drift workflow allows with the contract diff, which
# write-vendor-drift-report.sh caps well below that. Counted in bytes, whatever the locale.
readonly ACP_LINE_CUT=1000

# Reviewed mentions: `vendor<TAB>exact line text<TAB>reason`; `#` starts a comment. Tests point
# ACP_DOCS_ALLOWLIST at their own file.
ACP_DOCS_ALLOWLIST="${ACP_DOCS_ALLOWLIST:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/vendor-acp-docs-allowlist.txt}"

# The assessment that records each accepted mention, and the section of it that does. Tests point
# ACP_CONVERGENCE_DOC at their own file.
ACP_CONVERGENCE_DOC="${ACP_CONVERGENCE_DOC:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/docs/acp-convergence.md}"
readonly ACP_RECORD_HEADING='## Accepted documentation mentions'

usage() {
  echo 'usage: scripts/check-vendor-acp-docs.sh <claude|codex> [report-file]' >&2
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

# Prints the accepted line text of one vendor, one per line. The whole file is validated, so a
# malformed entry for another vendor is refused too instead of surfacing weeks later.
accepted_lines() {
  local vendor="$1"
  if [ ! -f "$ACP_DOCS_ALLOWLIST" ]; then
    printf 'expected allowlist file %s, received none\n' "$ACP_DOCS_ALLOWLIST" >&2
    return 2
  fi
  awk -F'\t' -v vendor="$vendor" -v path="$ACP_DOCS_ALLOWLIST" '
    /^[[:space:]]*(#|$)/ { next }
    ($1 != "claude" && $1 != "codex") || $2 == "" || $3 == "" || NF != 3 {
      printf "expected %s line %d as vendor<TAB>line text<TAB>reason with vendor claude or codex, received: %s\n", path, NR, $0 > "/dev/stderr"
      bad = 1
      next
    }
    $1 == vendor { print $2 }
    END { exit bad ? 2 : 0 }
  ' "$ACP_DOCS_ALLOWLIST"
}

# Prints the lines of the assessment section that records accepted mentions: from the heading to
# the next level-2 heading, outside code fences, so a fenced page line that starts with `## ` does
# not end the section early. Prints nothing when the heading is absent.
recorded_section() {
  awk -v heading="$ACP_RECORD_HEADING" '
    /^```/ { fenced = !fenced }
    !fenced && /^## / { in_section = ($0 == heading); next }
    in_section { print }
  ' "$ACP_CONVERGENCE_DOC"
}

# Fails (1) and names each allowlist entry, of either vendor, whose line text is not a whole line of
# the "Accepted documentation mentions" section of the assessment. The allowlist header asks that
# the decision be recorded there; this is what holds the two together. Exit 2: the assessment is
# missing or the allowlist is malformed.
unrecorded_allowlist_entries() {
  local vendor
  local entries
  local section
  local missing
  if [ ! -f "$ACP_CONVERGENCE_DOC" ]; then
    printf 'expected assessment file %s, received none\n' "$ACP_CONVERGENCE_DOC" >&2
    return 2
  fi
  entries=$(for vendor in claude codex; do accepted_lines "$vendor" || exit 2; done) || return 2
  [ -n "$entries" ] || return 0
  section=$(mktemp "${TMPDIR:-/tmp}/mea-vendor-acp-record.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -f -- $(printf '%q' "$section")" RETURN
  recorded_section > "$section"
  missing=$(printf '%s\n' "$entries" | grep -Fxv -f "$section" | cut -c 1-200 || true)
  if [ -n "$missing" ]; then
    printf 'expected every entry in %s to be a whole line of the "%s" section of %s, received no record for:\n%s\n' \
      "$ACP_DOCS_ALLOWLIST" "${ACP_RECORD_HEADING#\#\# }" "$ACP_CONVERGENCE_DOC" "$missing" >&2
    return 1
  fi
}

# Appends one section to the report file, if there is one: the page URL, a sentence, and optional
# page text in a code fence. Page text arrives prefixed with its line number, so a page line cannot
# open or close a fence. Only public page text goes in; curl output never does.
report_section() {
  local report_file="$1"
  local url="$2"
  local sentence="$3"
  local text="${4:-}"
  if [ -z "$report_file" ]; then
    return 0
  fi
  {
    printf '### %s\n\n%s\n' "$url" "$sentence"
    if [ -n "$text" ]; then
      # shellcheck disable=SC2016 # the backticks are literal Markdown fence characters, not command substitution
      printf '\n```text\n%s\n```\n' "$text"
    fi
    printf '\n'
  } >> "$report_file"
}

# Optional 4th argument: file of accepted line texts. Optional 5th: file that collects the accepted
# lines this document used, so the caller can find entries nothing matched. Optional 6th: report
# file that gains a section for this document when it fails.
validate_document() {
  local url="$1"
  local anchor="$2"
  local document="$3"
  local accepted_file="${4:-}"
  local seen_file="${5:-}"
  local report_file="${6:-}"
  local numbered
  local text
  local unaccepted=()
  local matches
  local shown
  if [ ! -f "$document" ]; then
    printf 'expected document file %s for %s, received none\n' "$document" "$url" >&2
    return 2
  fi
  if ! grep -Fq -- "$anchor" "$document"; then
    printf 'expected %s to contain %s, received no such text\n' "$url" "$anchor" >&2
    report_section "$report_file" "$url" "The page no longer contains the text \`$anchor\`: it may have moved or been replaced."
    return 2
  fi
  while IFS= read -r numbered; do
    text="${numbered#*:}"
    text="${text%$'\r'}"
    if [ -n "$accepted_file" ] && grep -Fxq -- "$text" "$accepted_file"; then
      if [ -n "$seen_file" ]; then
        printf '%s\n' "$text" >> "$seen_file"
      fi
    else
      unaccepted+=("$numbered")
    fi
  done < <(grep -nEi -- "$ACP_PATTERN" "$document" || true)
  if [ "${#unaccepted[@]}" -gt 0 ]; then
    shown=3
    if [ "${#unaccepted[@]}" -lt "$shown" ]; then
      shown="${#unaccepted[@]}"
    fi
    matches=$(printf '%s\n' "${unaccepted[@]:0:3}" | LC_ALL=C cut -c "1-$ACP_LINE_CUT")
    printf 'expected no ACP page or command in %s, received:\n%s\n' "$url" "$matches" >&2
    report_section "$report_file" "$url" \
      "Lines naming ACP that no reviewed allowlist entry accepts, with their line numbers (showing $shown of ${#unaccepted[@]}; each line is cut at $ACP_LINE_CUT bytes):" "$matches"
    return 1
  fi
  printf 'no ACP mention in %s\n' "$url"
}

fetch_and_validate_document() {
  local url="$1"
  local anchor="$2"
  local accepted_file="${3:-}"
  local seen_file="${4:-}"
  local report_file="${5:-}"
  local document
  local status
  document=$(mktemp "${TMPDIR:-/tmp}/mea-vendor-acp-doc.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -f -- $(printf '%q' "$document")" RETURN
  if ! curl --fail --silent --show-error --location "$url" --output "$document"; then
    printf 'expected to read %s, received a failed download\n' "$url" >&2
    report_section "$report_file" "$url" 'The page could not be downloaded.'
    return 2
  fi
  if validate_document "$url" "$anchor" "$document" "$accepted_file" "$seen_file" "$report_file"; then
    status=0
  else
    status=$?
  fi
  trap - RETURN
  rm -f -- "$document"
  return "$status"
}

# Appends the stale allowlist entries to the report file, if there is one. The entries are public
# page lines committed to this repository.
report_stale_entries() {
  local report_file="$1"
  local stale="$2"
  if [ -z "$report_file" ]; then
    return 0
  fi
  {
    printf '### Reviewed allowlist\n\n'
    printf 'Entries that no watched page carries any more. Remove or update them:\n\n'
    # Numbered like the page lines so that none of them can open or close the fence.
    # shellcheck disable=SC2016 # the backticks are literal Markdown fence characters, not command substitution
    printf '```text\n%s\n```\n\n' "$(printf '%s\n' "$stale" | awk '{ print NR ":" $0 }')"
  } >> "$report_file"
}

# Reads every document before it answers, so one run lists each page that needs triage. An
# unreadable page (2) outranks a mention (1): "could not verify" is the stronger statement. An
# allowlist entry that no page used is reported only when every page was read, since an unread page
# could still carry the line.
check_vendor() {
  local vendor="$1"
  local report_file="${2:-}"
  local documents
  local url
  local anchor
  local status
  local worst=0
  local scratch
  local stale
  documents=$(vendor_documents "$vendor") || return $?
  if [ -n "$report_file" ]; then
    # A report from an earlier run must not outlive a passing one.
    mkdir -p "$(dirname "$report_file")"
    rm -f -- "$report_file"
  fi
  scratch=$(mktemp -d "${TMPDIR:-/tmp}/mea-vendor-acp-allowlist.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -rf -- $(printf '%q' "$scratch")" RETURN
  accepted_lines "$vendor" > "$scratch/accepted" || return $?
  : > "$scratch/seen"
  while IFS='|' read -r url anchor; do
    if fetch_and_validate_document "$url" "$anchor" "$scratch/accepted" "$scratch/seen" "$report_file"; then
      status=0
    else
      status=$?
    fi
    if [ "$status" -eq 2 ] || { [ "$status" -eq 1 ] && [ "$worst" -eq 0 ]; }; then
      worst=$status
    fi
  done <<< "$documents"
  if [ "$worst" -ne 2 ]; then
    stale=$(grep -Fxv -f "$scratch/seen" "$scratch/accepted" | cut -c 1-200 || true)
    if [ -n "$stale" ]; then
      printf 'expected every %s entry in %s to match a line in a watched page, received no match for:\n%s\n' \
        "$vendor" "$ACP_DOCS_ALLOWLIST" "$stale" >&2
      report_stale_entries "$report_file" "$stale"
      worst=1
    fi
  fi
  return "$worst"
}

main() {
  if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    usage
    return 2
  fi
  check_vendor "$@"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
