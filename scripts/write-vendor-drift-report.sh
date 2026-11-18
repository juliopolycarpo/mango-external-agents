#!/usr/bin/env bash
# Writes a GitHub issue body when a fresh public contract differs from the pinned contract.
#
# Exit status 0 means no drift. Exit status 1 means a report was written. Exit status 2 means the
# comparison itself could not be made.
#
# Usage: scripts/write-vendor-drift-report.sh <vendor> <committed-path> <observed-path> <report-file>
#        scripts/write-vendor-drift-report.sh --failure <vendor> <stage> <report-file>
#        scripts/write-vendor-drift-report.sh --docs <vendor> <docs-report> <report-file>
#        scripts/write-vendor-drift-report.sh --help-probe <vendor> <probe-report> <report-file>
set -euo pipefail

# The diff shares the 65536-byte report bound the drift workflow enforces with the documentation
# watch section appended after it (up to six page lines of check-vendor-acp-docs.sh's ACP_LINE_CUT
# bytes each, plus the stale allowlist entries), so it leaves them room.
readonly MAX_DIFF_BYTES=48000

# shellcheck source=vendor-contract-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/vendor-contract-lib.sh"

usage() {
  echo "usage: scripts/write-vendor-drift-report.sh <vendor> <committed-path> <observed-path> <report-file>" >&2
  echo "       scripts/write-vendor-drift-report.sh --failure <vendor> <stage> <report-file>" >&2
  echo "       scripts/write-vendor-drift-report.sh --docs <vendor> <docs-report> <report-file>" >&2
  echo "       scripts/write-vendor-drift-report.sh --help-probe <vendor> <probe-report> <report-file>" >&2
}

write_report() {
  local vendor="$1"
  local committed="$2"
  local observed="$3"
  local report="$4"
  local temporary
  local status
  local bytes

  validate_contract_path 'committed contract' "$committed" || return $?
  validate_contract_path 'observed contract' "$observed" || return $?
  temporary=$(mktemp "${TMPDIR:-/tmp}/mea-vendor-diff.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -f -- $(printf '%q' "$temporary")" RETURN

  if git diff --no-index --no-ext-diff -- "$committed" "$observed" > "$temporary"; then
    status=0
  else
    status=$?
  fi
  case "$status" in
    0)
      trap - RETURN
      rm -f -- "$temporary"
      return 0
      ;;
    1) ;;
    *)
      echo "expected Git to compare $vendor contracts, received exit status $status" >&2
      trap - RETURN
      rm -f -- "$temporary"
      return 2
      ;;
  esac

  mkdir -p "$(dirname "$report")"
  bytes=$(wc -c < "$temporary")
  {
    echo "<!-- vendor-drift:$vendor -->"
    echo "# Vendor drift: $vendor"
    echo
    echo "The weekly drift lane reproduced the public, unauthenticated contract with the newest"
    echo "vendor release and found a difference from the committed pin. Historical transcripts and"
    echo "other archival captures are intentionally outside this comparison."
    echo
    echo '```diff'
    head -c "$MAX_DIFF_BYTES" "$temporary"
    if [ "$bytes" -gt "$MAX_DIFF_BYTES" ]; then
      echo
      echo "... diff truncated after $MAX_DIFF_BYTES bytes; download the workflow logs for the rest."
    fi
    echo '```'
  } > "$report"
  trap - RETURN
  rm -f -- "$temporary"
  return 1
}

write_failure_report() {
  local vendor="$1"
  local stage="$2"
  local report="$3"

  mkdir -p "$(dirname "$report")"
  {
    echo "<!-- vendor-drift:$vendor -->"
    echo "# Vendor drift: $vendor"
    echo
    echo "The weekly drift lane could not complete the $stage stage for the newest vendor release."
    echo "This is treated as drift because the current public contract could not be verified."
    echo
    echo "Inspect this workflow run for the vendor command output and update the harness or the"
    echo "contract capture after reviewing the documented vendor change."
  } > "$report"
}

# Adds one watch result to the report file. A report that already exists (the contract diff, or
# another watch) keeps its text and gains a section; otherwise this writes a new report. `watch_report`
# is what the watch wrote: public page or help lines and the names of what was read. When the watch
# failed before it wrote one (a malformed allowlist), the section says so and points at the run.
# The texts are the section heading, the two lines under it, and the note for a missing report,
# which says what that watch reads (pages or commands).
append_watch_report() {
  local vendor="$1"
  local watch_report="$2"
  local report="$3"
  local heading="$4"
  local first_line="$5"
  local second_line="$6"
  local missing_detail="$7"

  mkdir -p "$(dirname "$report")"
  if [ ! -f "$report" ]; then
    {
      echo "<!-- vendor-drift:$vendor -->"
      echo "# Vendor drift: $vendor"
    } > "$report"
  fi
  {
    echo
    echo "## $heading"
    echo
    echo "$first_line"
    echo "$second_line"
    echo
    if [ -f "$watch_report" ] && [ ! -L "$watch_report" ]; then
      cat -- "$watch_report"
    else
      echo "$missing_detail"
    fi
  } >> "$report"
}

# The documentation watch result: what `check-vendor-acp-docs.sh` wrote.
write_docs_report() {
  append_watch_report "$1" "$2" "$3" 'Documentation watch' \
    'The documentation watch for an ACP page or command failed in this run: a vendor document' \
    'now mentions ACP outside the reviewed allowlist, or could not be read.' \
    'The watch wrote no page detail. Inspect this workflow run for its output.'
}

# The subcommand help watch result: what `check-vendor-subcommand-help.sh` wrote.
write_help_probe_report() {
  append_watch_report "$1" "$2" "$3" 'Subcommand help watch' \
    'The subcommand help watch for an ACP flag or command failed in this run: the help of a' \
    'subcommand now names ACP, or could not be read.' \
    'The watch wrote no command detail. Inspect this workflow run for its output.'
}

main() {
  if [ $# -gt 0 ] && [ "$1" = '--docs' ]; then
    if [ $# -ne 4 ]; then
      usage
      return 2
    fi
    write_docs_report "$2" "$3" "$4"
    return
  fi
  if [ $# -gt 0 ] && [ "$1" = '--help-probe' ]; then
    if [ $# -ne 4 ]; then
      usage
      return 2
    fi
    write_help_probe_report "$2" "$3" "$4"
    return
  fi
  if [ $# -gt 0 ] && [ "$1" = '--failure' ]; then
    if [ $# -ne 4 ]; then
      usage
      return 2
    fi
    write_failure_report "$2" "$3" "$4"
    return
  fi
  if [ $# -ne 4 ]; then
    usage
    return 2
  fi
  write_report "$1" "$2" "$3" "$4"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
