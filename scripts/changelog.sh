#!/usr/bin/env bash
# Regenerate CHANGELOG.md from Conventional Commits. Never edit the file by hand.
# Usage: scripts/changelog.sh            # unreleased section on top
#        scripts/changelog.sh v0.2.0     # as if HEAD were tagged v0.2.0
set -euo pipefail
cd "$(dirname "$0")/.."
if [ $# -gt 0 ]; then
  git-cliff --config cliff.toml --tag "$1" --output CHANGELOG.md
else
  git-cliff --config cliff.toml --output CHANGELOG.md
fi
