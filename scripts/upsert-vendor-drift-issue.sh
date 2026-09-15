#!/usr/bin/env bash
# Maintains one open vendor-drift issue for one vendor.
#
# The scheduled workflow is the only caller. Keeping this stateful operation out of the workflow
# YAML makes its deduplication rule testable and keeps pull-request jobs read-only.
#
# Usage: scripts/upsert-vendor-drift-issue.sh <owner/repo> <vendor> <report-file>
set -euo pipefail

usage() {
  echo "usage: scripts/upsert-vendor-drift-issue.sh <owner/repo> <vendor> <report-file>" >&2
}

issue_title() {
  printf 'drift(%s): public contract changed\n' "$1"
}

open_issue_number() {
  local repository="$1"
  local vendor="$2"
  local title
  local marker
  local candidates
  local number
  local candidate_title
  local body
  title=$(issue_title "$vendor")
  marker="<!-- vendor-drift:$vendor -->"
  if ! candidates=$(gh issue list \
    --repo "$repository" \
    --state open \
    --limit 100 \
    --search "in:title drift($vendor)" \
    --json number,title \
    --jq '.[] | [.number, .title] | @tsv'); then
    echo "could not list open drift issues for $vendor in $repository" >&2
    return 2
  fi
  while IFS=$'\t' read -r number candidate_title; do
    if [ "$candidate_title" != "$title" ]; then
      continue
    fi
    if ! body=$(gh issue view "$number" --repo "$repository" --json body --jq '.body'); then
      echo "could not inspect candidate drift issue #$number for $vendor" >&2
      return 2
    fi
    case "$body" in
      *"$marker"*)
        printf '%s\n' "$number"
        return
        ;;
    esac
  done <<EOF
$candidates
EOF
}

upsert_issue() {
  local repository="$1"
  local vendor="$2"
  local report="$3"
  local existing

  if [ ! -f "$report" ]; then
    printf 'expected report file %s, received none\n' "$report" >&2
    return 2
  fi

  gh label create 'type: drift' \
    --repo "$repository" \
    --color B60205 \
    --description 'A vendor CLI contract changed' \
    --force
  if ! existing=$(open_issue_number "$repository" "$vendor"); then
    return 2
  fi
  if [ -n "$existing" ]; then
    gh issue edit "$existing" --repo "$repository" --body-file "$report"
    echo "updated vendor drift issue #$existing for $vendor"
    return
  fi

  gh issue create \
    --repo "$repository" \
    --title "$(issue_title "$vendor")" \
    --label 'type: drift' \
    --body-file "$report"
}

main() {
  if [ $# -ne 3 ]; then
    usage
    return 2
  fi
  upsert_issue "$1" "$2" "$3"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
