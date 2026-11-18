#!/usr/bin/env bash
# Watches each subcommand's own help for an ACP flag or command.
#
# The drift lane records the top-level help of each vendor CLI and reads the vendor documentation
# (scripts/check-vendor-acp-docs.sh), but a flag a release adds under `claude mcp`, `claude auth`,
# `codex exec` or another subcommand shows in neither until a watched page names it. This watch runs
# `<cli> <subcommand> --help` for every command the freshly captured top-level help lists, searches
# the text for ACP and discards it. Nothing is committed: each subcommand help would otherwise be a
# contract that has to read the same on Ubuntu, macOS and Windows, and a rewording of a command the
# library never calls would open a drift issue. See docs/vendor-drift.md.
#
# The probe is read-only. It needs no login and spawns only the CLI the lane installed, once per
# command, with `--help` as the only argument, under a scratch home so that nothing of the caller's
# own configuration is read or written.
#
# With a report file, the watch also writes what a maintainer needs to triage without opening the
# run log: each command that needs attention and the matching help lines with their line numbers
# (public help text only, never anything else the CLI printed). The drift workflow appends it to the
# managed issue body; the file does not exist when the watch passes.
#
# Exit status 0: no subcommand help names ACP. 1: at least one does. 2: the watch could not verify:
# the top-level help lists no command, a command name is not one, or a subcommand's help could not
# be read or did not answer in time. A watch that could not look is not a clean one.
#
# VENDOR_HELP_CLI names the program to run in place of the vendor's own name; tests point it at a
# named fake. HELP_TIMEOUT_SECONDS (default 30) bounds each `--help`, so a CLI that waits on a
# prompt or the network cannot hold the lane until the workflow step's own limit.
#
# Usage: scripts/check-vendor-subcommand-help.sh <claude|codex> <top-level-help-file> [report-file]
set -euo pipefail

# ACP_PATTERN and cut_lines_to_report_bytes, shared with the documentation watch.
# shellcheck source=vendor-acp-lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/vendor-acp-lib.sh"

# A report is bounded by the drift workflow at 65536 bytes, and the contract diff (48000) and the
# documentation watch section already take most of it. This watch adds at most six commands of three
# lines each, cut at 240 bytes: a help line is short, so the cut costs no real text.
readonly HELP_COMMANDS_SHOWN=6
readonly HELP_LINES_SHOWN=3
readonly HELP_LINE_CUT=240

help_probe_usage() {
  echo 'usage: scripts/check-vendor-subcommand-help.sh <claude|codex> <top-level-help-file> [report-file]' >&2
}

# Prints the subcommand names of a top-level help file, one per line: the first word of each entry
# of its `Commands:` section, up to the first `|` (Claude lists `plugin|plugins`), without `help`,
# which only prints the top-level help again. An entry is a line indented by exactly two spaces;
# the deeper lines are a wrapped description. The section ends at a blank line or at the next
# unindented heading.
help_subcommands() {
  awk '
    /^Commands:[[:space:]]*$/ { in_commands = 1; next }
    in_commands && (/^$/ || /^[^[:space:]]/) { in_commands = 0 }
    in_commands && /^  [^[:space:]]/ {
      name = $1
      sub(/\|.*/, "", name)
      if (name != "help") print name
    }
  ' "$1"
}

# Appends one section to the report file, if there is one: the command line that was run, a
# sentence, and optional help text in a code fence. Help text arrives prefixed with its line number,
# so a help line cannot open or close the fence. Only public help text goes in.
help_report_section() {
  local report_file="$1"
  local heading="$2"
  local sentence="$3"
  local text="${4:-}"
  if [ -z "$report_file" ]; then
    return 0
  fi
  {
    # shellcheck disable=SC2016 # the backticks are literal Markdown, not command substitution
    printf '### `%s`\n\n%s\n' "$heading" "$sentence"
    if [ -n "$text" ]; then
      # shellcheck disable=SC2016 # the backticks are literal Markdown fence characters, not command substitution
      printf '\n```text\n%s\n```\n' "$text"
    fi
    printf '\n'
  } >> "$report_file"
}

# Runs `<cli> <command> --help` under the scratch home, for at most $5 seconds, and leaves its
# output in $4. Returns the CLI's exit status, 124 or 137 when the limit cut it off, or 127 when it
# could not be started.
run_subcommand_help() {
  local cli="$1"
  local command="$2"
  local scratch="$3"
  local output="$4"
  local limit="$5"
  # `timeout` ends a command that never answers (a CLI waiting on a prompt or the network): TERM
  # after the limit, then KILL five seconds later. It exits 124 for the first and 137 for the second.
  timeout --kill-after=5 "$limit" env HOME="$scratch/home" \
    CLAUDE_CONFIG_DIR="$scratch/claude" \
    CODEX_HOME="$scratch/codex" \
    XDG_CONFIG_HOME="$scratch/xdg/config" \
    XDG_DATA_HOME="$scratch/xdg/data" \
    XDG_STATE_HOME="$scratch/xdg/state" \
    XDG_CACHE_HOME="$scratch/xdg/cache" \
    "$cli" "$command" --help < /dev/null > "$output" 2>&1
}

# Reads every subcommand before it answers, so one run lists each command that needs triage. An
# unreadable help (2) outranks a mention (1): "could not verify" is the stronger statement.
check_subcommand_help() {
  local vendor="$1"
  local help_file="$2"
  local report_file="${3:-}"
  local cli="${VENDOR_HELP_CLI:-$vendor}"
  local limit="${HELP_TIMEOUT_SECONDS:-30}"
  local commands
  local command
  local scratch
  local output
  local numbered
  local matches
  local status
  local worst=0
  local flagged=0
  local unshown=()

  case "$vendor" in
    claude | codex) ;;
    *)
      printf 'expected vendor claude or codex, received %s\n' "$vendor" >&2
      return 2
      ;;
  esac
  if ! [[ "$limit" =~ ^[1-9][0-9]*$ ]]; then
    printf 'expected HELP_TIMEOUT_SECONDS to be a whole number of seconds above zero, received %s\n' "$limit" >&2
    return 2
  fi
  if ! command -v timeout > /dev/null 2>&1; then
    printf 'expected the timeout command (GNU coreutils) to bound each --help, received none on PATH\n' >&2
    return 2
  fi
  if [ ! -f "$help_file" ]; then
    printf 'expected the top-level help file %s, received none\n' "$help_file" >&2
    return 2
  fi
  if [ -n "$report_file" ]; then
    # A report from an earlier run must not outlive a passing one.
    mkdir -p "$(dirname "$report_file")"
    rm -f -- "$report_file"
  fi
  commands=$(help_subcommands "$help_file")
  if [ -z "$commands" ]; then
    printf 'expected a Commands: section listing at least one subcommand in %s, received none\n' "$help_file" >&2
    help_report_section "$report_file" "$vendor --help" \
      'The top-level help lists no subcommand, so no subcommand help was searched. Its layout may have changed.'
    return 2
  fi
  while IFS= read -r command; do
    # The name goes to the CLI as an argument: refuse anything that is not a plain command word.
    if ! [[ "$command" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
      printf 'expected a subcommand name of letters, digits, dot, dash or underscore in %s, received %s\n' "$help_file" "$command" >&2
      help_report_section "$report_file" "$vendor --help" \
        'The top-level help lists a command name this watch does not read as one, so its help was not searched.'
      return 2
    fi
  done <<< "$commands"

  scratch=$(mktemp -d "${TMPDIR:-/tmp}/mea-vendor-subcommand-help.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -rf -- $(printf '%q' "$scratch")" RETURN
  mkdir -p "$scratch/home" "$scratch/claude" "$scratch/codex" \
    "$scratch/xdg/config" "$scratch/xdg/data" "$scratch/xdg/state" "$scratch/xdg/cache"
  output="$scratch/help.txt"

  while IFS= read -r command; do
    if run_subcommand_help "$cli" "$command" "$scratch" "$output" "$limit"; then
      status=0
    else
      status=$?
    fi
    if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
      printf 'expected %s %s --help to answer within %s seconds, received no answer\n' "$vendor" "$command" "$limit" >&2
      help_report_section "$report_file" "$vendor $command --help" \
        "The command did not answer within $limit seconds, so its help was not searched."
      worst=2
      continue
    fi
    if [ "$status" -ne 0 ]; then
      printf 'expected %s %s --help to exit 0, received exit status %s\n' "$vendor" "$command" "$status" >&2
      help_report_section "$report_file" "$vendor $command --help" \
        "The command exited with status $status, so its help was not searched."
      worst=2
      continue
    fi
    matches=$(grep -nEi -- "$ACP_PATTERN" "$output" || true)
    if [ -z "$matches" ]; then
      printf 'no ACP mention in %s %s --help\n' "$vendor" "$command"
      continue
    fi
    flagged=$((flagged + 1))
    numbered=$(printf '%s\n' "$matches" | sed -n "1,${HELP_LINES_SHOWN}p" | cut_lines_to_report_bytes "$HELP_LINE_CUT")
    printf 'expected no ACP flag or command in %s %s --help, received:\n%s\n' "$vendor" "$command" "$numbered" >&2
    if [ "$flagged" -le "$HELP_COMMANDS_SHOWN" ]; then
      help_report_section "$report_file" "$vendor $command --help" \
        "Lines naming ACP, with their line numbers (showing $(printf '%s\n' "$numbered" | wc -l | tr -d ' ') of $(printf '%s\n' "$matches" | wc -l | tr -d ' '); each line is cut at $HELP_LINE_CUT bytes):" "$numbered"
    else
      unshown+=("$command")
    fi
    if [ "$worst" -eq 0 ]; then
      worst=1
    fi
  done <<< "$commands"
  if [ "${#unshown[@]}" -gt 0 ]; then
    help_report_section "$report_file" "$vendor --help" \
      "Also naming ACP, not shown here: ${unshown[*]}."
  fi
  return "$worst"
}

main() {
  if [ $# -lt 2 ] || [ $# -gt 3 ]; then
    help_probe_usage
    return 2
  fi
  check_subcommand_help "$@"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
