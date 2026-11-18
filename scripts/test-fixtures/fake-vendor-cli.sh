#!/usr/bin/env bash
# Named fake for an installed vendor CLI (`claude`, `codex`) as the subcommand help watch drives it:
# `<command> --help` answers with the text of $FAKE_VENDOR_CLI_ROOT/<command>.help instead of
# running a vendor. A command with no file answers like a CLI that does not know it (exit 2).
# A command with a .hang file beside its help page never answers (see below).
# Every call is appended to $FAKE_VENDOR_CLI_LOG with the home variables it ran under, so a test can
# prove the watch ran nothing but `--help` and kept the CLI away from the caller's own state.
set -euo pipefail

root="${FAKE_VENDOR_CLI_ROOT:?expected FAKE_VENDOR_CLI_ROOT}"
log="${FAKE_VENDOR_CLI_LOG:?expected FAKE_VENDOR_CLI_LOG}"

printf 'argv=%s HOME=%s CLAUDE_CONFIG_DIR=%s CODEX_HOME=%s\n' \
  "$*" "${HOME:-}" "${CLAUDE_CONFIG_DIR:-}" "${CODEX_HOME:-}" >> "$log"

if [ $# -ne 2 ] || [ "$2" != '--help' ]; then
  printf 'fake vendor CLI: expected <command> --help, received %s\n' "$*" >&2
  exit 64
fi

# A command with a $FAKE_VENDOR_CLI_ROOT/<command>.hang file never answers, like a CLI waiting on a
# prompt or the network. `exec` keeps the hang in this process, so a timeout signal ends it.
if [ -f "$root/$1.hang" ]; then
  exec sleep 300
fi

page="$root/$1.help"
if [ ! -f "$page" ]; then
  printf 'error: unrecognized subcommand %s\n' "$1" >&2
  exit 2
fi
cat -- "$page"
