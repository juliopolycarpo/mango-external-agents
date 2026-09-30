#!/usr/bin/env bash
# Named fake for scripts/test-vendor-drift.sh. It records GitHub CLI calls without touching GitHub.
#
# `gh api` answers like the issues REST endpoint, and only for the query the drift script must send:
# open issues carrying the `type: drift` label, read with `--paginate`. Any other query gets an
# empty page, which is what GitHub answers for a search that matches nothing. The matching issue is
# on the second page and a pull request carrying the same title is on the first, so a script that
# stops after one page or keeps pull requests cannot find the right issue.
set -euo pipefail

printf '%s\n' "$*" >> "${FAKE_GH_LOG:?expected FAKE_GH_LOG}"

if [ "${FAKE_GH_FAILURE:-}" = "$1-$2" ]; then
  exit 1
fi

issue_pages() {
  local vendor="${FAKE_GH_VENDOR:?expected FAKE_GH_VENDOR}"
  local number="${FAKE_GH_ISSUE_NUMBER:-}"
  local title="drift($vendor): public contract changed"
  local pull_request_number=$((${number:-0} + 1000))
  local first_page
  local second_page
  first_page=$(jq -n -c --arg title "$title" --argjson number "$pull_request_number" \
    '[{number: $number, title: $title, pull_request: {}}, {number: 7, title: "drift(other): public contract changed"}]')
  second_page='[]'
  if [ -n "$number" ]; then
    second_page=$(jq -n -c --arg title "$title" --argjson number "$number" \
      '[{number: $number, title: $title}]')
  fi
  printf '%s\n' "$first_page" "$second_page"
}

answer_api() {
  local paginate=false
  local jq_filter='.'
  local endpoint=''
  while [ $# -gt 0 ]; do
    case "$1" in
      --paginate) paginate=true ;;
      --jq)
        shift
        jq_filter="$1"
        ;;
      repos/*) endpoint="$1" ;;
    esac
    shift
  done
  if [ "${FAKE_GH_FAILURE:-}" = 'issue-list' ]; then
    exit 1
  fi
  local expected='repos/owner/repository/issues?state=open&labels=type:+drift'
  local page
  local index=0
  case "$endpoint" in
    "$expected" | "$expected"'&per_page='*) ;;
    *) return ;;
  esac
  while IFS= read -r page; do
    if [ "$index" -gt 0 ] && [ "$paginate" = false ]; then
      break
    fi
    printf '%s\n' "$page" | jq -r "$jq_filter"
    index=$((index + 1))
  done < <(issue_pages)
}

case "$1 $2" in
  'issue view')
    printf '%s\n' "${FAKE_GH_ISSUE_BODY:-}"
    ;;
  api\ *)
    answer_api "$@"
    ;;
esac
