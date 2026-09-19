#!/usr/bin/env bash
# A pull request title is a commit subject. Check it reads as one.
#
# This repository squash-merges, and GitHub takes the squash subject from the PR title verbatim —
# but only while the repository's squash-title setting is `PR_TITLE`; under the default
# `COMMIT_OR_PR_TITLE` a single-commit pull request is squashed under that commit's own subject and
# this check guards nothing. `docs/releasing.md` records the setting this gate depends on.
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

# Both lists are read from the file that owns them rather than repeated here, so what a contributor
# is told to use and what this check enforces cannot drift apart. `|| true` on each grep because a
# pattern that matches nothing exits 1, and under `pipefail` that would kill the assignment before
# the diagnostic below could name the file that stopped answering.

# Collapse the matches into one space-separated line the `case` membership tests can scan.
as_list() { sort -u | tr '\n' ' ' | sed 's/[[:space:]]*$//'; }

# The types are exactly the ones cliff.toml groups: a type it does not parse lands the pull request
# in `Other`. `^Initial commit` is not matched — the class is lower-case only.
types=$(
  { grep -oE 'message = "\^[a-z]+' cliff.toml || true; } | sed 's/.*\^//' | as_list
)
if [ -z "$types" ]; then
  echo "expected '^<type>' message parsers in cliff.toml's commit_parsers, received none" >&2
  exit 1
fi

# The scopes are the backticked words of the `Scopes:` sentence in AGENTS.md's commit rule, and
# nothing else in that bullet: matching the whole bullet would turn any backticked lower-case word
# a later edit adds to its prose into an accepted scope.
scopes=$(
  sed -n '/^- Conventional Commits with a body/,/per commit\./p' AGENTS.md | tr '\n' ' ' |
    { grep -oE 'Scopes:[^.]*\.' || true; } |
    { grep -oE '`[a-z]+`' || true; } | tr -d '`' | as_list
)
if [ -z "$scopes" ]; then
  echo "expected a 'Scopes:' sentence in AGENTS.md's commit rules, received none" >&2
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

# type, optional (scope), optional !, colon, space, non-empty summary. The scope is `[^)]+`, not
# `[^)]*`: `feat(): x` would otherwise pass — an empty scope skips the membership check below, and
# git-cliff does not read it as conventional, so the entry renders as `- Feat(): x`. Held in a
# variable because the character class ends in `]]`, which closes the conditional if the pattern is
# written inline.
subject='^([a-z]+)(\(([^)]+)\))?(!)?:[[:space:]](.+)$'
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
# Matched against the whole class, not against spaces alone: a tab is whitespace too.
if [[ "$summary" =~ ^[[:space:]]*$ ]]; then
  fail "expected a summary after the colon, received only whitespace"
fi

echo "pull request title reads as a commit subject: $type${scope:+($scope)}"
