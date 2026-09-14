#!/usr/bin/env bash
# Compares one public, deterministic vendor contract with a fresh capture.
#
# Usage: scripts/check-vendor-contract.sh <vendor> <committed-dir> <captured-dir>
set -euo pipefail

# shellcheck source=vendor-contract-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/vendor-contract-lib.sh"

usage() {
  echo "usage: scripts/check-vendor-contract.sh <vendor> <committed-dir> <captured-dir>" >&2
}

compare_contract() {
  local vendor="$1"
  local committed="$2"
  local captured="$3"
  local status
  validate_contract_path 'committed contract' "$committed" || return $?
  validate_contract_path 'captured contract' "$captured" || return $?

  if git diff --no-index --no-ext-diff -- "$committed" "$captured"; then
    status=0
  else
    status=$?
  fi
  case "$status" in
    0)
      echo "public $vendor contract matches"
      return 0
      ;;
    1)
      echo "public $vendor contract changed; refresh the capture after reviewing the vendor change" >&2
      return 1
      ;;
    *)
      echo "expected Git to compare $vendor contracts, received exit status $status" >&2
      return 2
      ;;
  esac
}

main() {
  if [ $# -ne 3 ]; then
    usage
    return 2
  fi
  compare_contract "$1" "$2" "$3"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
