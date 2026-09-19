#!/usr/bin/env bash
# A pull request title is a commit subject. Check it reads as one.
#
# This repository squash-merges, and GitHub takes the squash subject from the PR title verbatim.
# git-cliff parses that subject to build CHANGELOG.md and the GitHub release notes, so a title
# without a Conventional Commit type does not merely look untidy: the whole pull request is filed
# under `Other` with no scope, and before the fix that came with this script it vanished from both
# files entirely. v0.1.0 shipped release notes with no mention of the pull request that produced the
# tagged tree. Nothing caught it until the release was out, which is what this script is for.
#
# Usage: scripts/check-pr-title.sh "feat(core): add a thing"
set -euo pipefail
cd "$(dirname "$0")/.."

title=${1-}
if [ -z "$title" ]; then
  echo "expected a pull request title as the first argument, received none" >&2
  exit 2
fi

# The Conventional Commits types this repository releases from; they are the ones cliff.toml groups.
types="build chore ci docs feat fix migration perf refactor revert security style test"

# Read from AGENTS.md rather than repeated here, so the list a contributor is told to use and the
# list this check enforces cannot drift apart. The commit-rule bullet names every scope in
# backticks; `type(scope): summary` and `examples/` carry characters outside the class and so are
# not mistaken for scopes.
scopes=$(
  sed -n '/^- Conventional Commits with a body/,/per commit\./p' AGENTS.md |
    grep -o '`[a-z]*`' | tr -d '`' | sort -u | tr '\n' ' '
)
if [ -z "$scopes" ]; then
  echo "expected a 'Scopes:' list in AGENTS.md's commit rules, received none" >&2
  exit 1
fi

fail() {
  echo "$1" >&2
  echo "received: $title" >&2
  echo "expected: type(scope): summary, with an optional ! before the colon" >&2
  echo "  types:  $types" >&2
  echo "  scopes: $scopes" >&2
  exit 1
}

# type, optional (scope), optional !, colon, space, non-empty summary. Held in a variable because
# the character class ends in `]]`, which closes the conditional if the pattern is written inline.
subject='^([a-z]+)(\(([^)]*)\))?(!)?:[[:space:]](.+)$'
if [[ ! "$title" =~ $subject ]]; then
  fail "expected a Conventional Commit subject"
fi
type=${BASH_REMATCH[1]}
scope=${BASH_REMATCH[3]}
summary=${BASH_REMATCH[5]}

case " $types " in
  *" $type "*) ;;
  *) fail "expected a known commit type, received '$type'" ;;
esac

if [ -n "$scope" ]; then
  case " $scopes " in
    *" $scope "*) ;;
    *) fail "expected a known scope, received '$scope'" ;;
  esac
fi

# A summary that is only whitespace passes the pattern above but produces an empty changelog entry.
if [ -z "${summary// /}" ]; then
  fail "expected a summary after the colon, received only whitespace"
fi

echo "pull request title reads as a commit subject: $type${scope:+($scope)}"
