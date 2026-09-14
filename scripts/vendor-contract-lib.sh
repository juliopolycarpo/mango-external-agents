#!/usr/bin/env bash
# Checks shared by the two public-contract comparators.
#
# `scripts/check-vendor-contract.sh` fails a pull request on drift and `scripts/write-vendor-drift-report.sh`
# turns the same comparison into an issue body. Both refuse a path they were handed rather than
# reporting an empty tree as a match, and a rule written twice is a rule that can disagree with
# itself — `scripts/test-vendor-drift.sh` sources both files, so the second copy would silently
# shadow the first and the divergence would never fail a test.

validate_contract_path() {
  local label="$1"
  local path="$2"
  if [ ! -e "$path" ]; then
    printf 'expected %s path %s, received none\n' "$label" "$path" >&2
    return 2
  fi
}
