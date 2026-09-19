#!/usr/bin/env bash
# Regression coverage for tag-only dispatch and mismatched release versions.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(scripts/release-version.sh refs/tags/v0.1.0)" == '0.1.0' ]]
[[ "$(scripts/release-version.sh refs/tags/v0.2.0-rc.1 0.2.0-rc.1)" == '0.2.0-rc.1' ]]
for ref in refs/heads/main refs/heads/v0.1.0 refs/tags/v0.1 refs/tags/v0.1.0-canary.1; do
  if scripts/release-version.sh "$ref" 0.1.0; then
    echo "expected rejection of non-release ref, received success for $ref" >&2
    exit 1
  fi
done
if scripts/release-version.sh refs/tags/v0.1.0 0.2.0; then
  echo 'expected rejection of mismatched version, received success' >&2
  exit 1
fi
echo 'release tag checks passed'

# A pull request title becomes the squash subject, and git-cliff reads that subject. The rejected
# case below is verbatim the title that shipped v0.1.0's release notes without the pull request
# that produced the tagged tree.
for title in 'feat(core): add a thing' 'fix: no scope' 'docs(release): record the bootstrap' \
  'feat(acp)!: breaking change' 'chore(deps): bump a dependency' \
  'migration(fixtures): a type only cliff.toml names'; do
  if ! scripts/check-pr-title.sh "$title" >/dev/null; then
    echo "expected acceptance of '$title', received the rejection above" >&2
    exit 1
  fi
done
while IFS= read -r title; do
  if scripts/check-pr-title.sh "$title" >/dev/null 2>&1; then
    echo "expected rejection of '$title', received success" >&2
    exit 1
  fi
done <<'TITLES'
Release gate: host adoption, Hub-owned retry, and publication readiness (#18)
feat(nope): a scope AGENTS.md does not name
nope(core): a type the changelog cannot group
feat(core):no space after the colon
feat(core):
feat(): an empty scope is not a scope
TITLES
# Separate from the list above because a tab cannot survive being read back as literal text. The
# space before the tab is load-bearing: `[[:space:]]` in the subject pattern consumes one character,
# so `feat(core):<tab>` is rejected by the pattern itself and would pass this test against the
# whitespace guard it is meant to exercise. With the space, the summary is the tab alone.
tab_title=$(printf 'feat(core): \t')
if rejection=$(scripts/check-pr-title.sh "$tab_title" 2>&1); then
  echo 'expected rejection of a summary that is only a tab, received success' >&2
  exit 1
fi
case "$rejection" in
  *'received only whitespace'*) ;;
  *)
    echo 'expected the whitespace diagnostic for a tab-only summary, received:' >&2
    echo "$rejection" >&2
    exit 1
    ;;
esac
if scripts/check-pr-title.sh >/dev/null 2>&1; then
  echo 'expected rejection of an empty title, received success' >&2
  exit 1
fi
echo 'pull request title checks passed'

# What the title gate protects: git-cliff has to keep a squash subject that carries no Conventional
# Commit type, show only its first line, and mark a breaking change. `filter_unconventional = true`
# — the setting that kept #18 out of v0.1.0's notes — fails the first assertion, dropping the
# `split` filter fails the second, and dropping the `commit.breaking` branch fails the third.
if ! command -v git-cliff >/dev/null 2>&1; then
  echo 'skipping the changelog regression: git-cliff is not on PATH' >&2
else
  config=$PWD/cliff.toml
  fixture=$(mktemp -d)
  trap 'rm -rf "$fixture"' EXIT
  commit() {
    git -C "$fixture" -c user.name=fixture -c user.email=fixture@example.invalid \
      -c commit.gpgsign=false commit -q --allow-empty "$@"
  }
  git init -q -b main "$fixture"
  commit -m 'feat(core)!: a breaking change' -m 'A body.'
  commit -m 'Release gate: a subject with no type (#18)' \
    -m 'A body line the changelog must not repeat.'
  notes=$(git-cliff --config "$config" --repository "$fixture" 2>/dev/null)
  for expected in '- Release gate: a subject with no type (#18)' \
    '- [**breaking**] **(core)** A breaking change'; do
    case "$notes" in
      *"$expected"*) ;;
      *)
        echo "expected the changelog to contain '$expected', received:" >&2
        echo "$notes" >&2
        exit 1
        ;;
    esac
  done
  case "$notes" in
    *'A body line the changelog must not repeat.'*)
      echo 'expected the changelog to carry subjects only, received a commit body:' >&2
      echo "$notes" >&2
      exit 1
      ;;
  esac
  rm -rf "$fixture"
  trap - EXIT
  echo 'changelog rendering checks passed'
fi
