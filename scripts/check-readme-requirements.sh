#!/usr/bin/env bash
# Every `mango-* = "…"` install requirement in a crate README names the release being cut.
#
# A published README is immutable, and 0.3.0 shipped `"0.1"` — a caret requirement that never
# resolves to 0.3. The rule: a release names `major.minor` (`"0.3"` for 0.3.1); a pre-release names
# its full version (`"0.4.0-rc.1"`), because no caret requirement resolves to a pre-release.
#
# `scripts/check-versions.sh` runs this over `crates/*/README.md`. It takes the files as arguments
# so the regression test can point it at fixtures.
# Usage: scripts/check-readme-requirements.sh <workspace-version> <README.md>...
set -euo pipefail

if [ $# -lt 2 ]; then
  echo "expected <workspace-version> <README.md>..., received $# argument(s)" >&2
  exit 2
fi
version=$1
shift

if [[ ! "$version" =~ ^([0-9]+\.[0-9]+)\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "expected a version like 0.3.1 or 0.4.0-rc.1, received '$version'" >&2
  exit 2
fi
if [[ -n "${BASH_REMATCH[2]}" ]]; then
  required=$version
else
  required=${BASH_REMATCH[1]}
fi

status=0
for readme in "$@"; do
  if [ ! -f "$readme" ]; then
    echo "$readme: expected a README file, received none" >&2
    status=1
    continue
  fi
  found=0
  lineno=0
  while IFS= read -r line; do
    lineno=$((lineno + 1))
    if [[ "$line" =~ ^(mango-[a-z-]+)[[:space:]]*=[[:space:]]*\"([^\"]*)\"[[:space:]]*$ ]]; then
      found=$((found + 1))
      if [ "${BASH_REMATCH[2]}" != "$required" ]; then
        echo "$readme:$lineno: expected ${BASH_REMATCH[1]} = \"$required\" for workspace version $version, received \"${BASH_REMATCH[2]}\"" >&2
        status=1
      fi
    fi
  done < "$readme"
  if [ "$found" -eq 0 ]; then
    echo "$readme: expected a 'mango-… = \"$required\"' install requirement, received none" >&2
    status=1
  fi
done
exit $status
