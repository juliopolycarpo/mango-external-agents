#!/usr/bin/env bash
# Writes a GitHub issue body when a fresh public contract differs from the pinned contract.
#
# Exit status 0 means no drift. Exit status 1 means a report was written. Exit status 2 means the
# comparison itself could not be made.
#
# Usage: scripts/write-vendor-drift-report.sh <vendor> <committed-path> <observed-path> <report-file>
#        scripts/write-vendor-drift-report.sh --failure <vendor> <stage> <report-file>
set -euo pipefail

readonly MAX_DIFF_BYTES=60000

usage() {
  echo "usage: scripts/write-vendor-drift-report.sh <vendor> <committed-path> <observed-path> <report-file>" >&2
  echo "       scripts/write-vendor-drift-report.sh --failure <vendor> <stage> <report-file>" >&2
}

validate_contract_path() {
  local label="$1"
  local path="$2"
  if [ ! -e "$path" ]; then
    printf 'expected %s path %s, received none\n' "$label" "$path" >&2
    return 2
  fi
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

main() {
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
