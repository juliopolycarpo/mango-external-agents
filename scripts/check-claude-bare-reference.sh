#!/usr/bin/env bash
# Watches Anthropic's documented assertion that --bare is opt-in for non-interactive Claude Code.
#
# Usage: scripts/check-claude-bare-reference.sh [--file PATH]
set -euo pipefail

readonly CLAUDE_HEADLESS_REFERENCE='https://code.claude.com/docs/en/headless'

usage() {
  echo 'usage: scripts/check-claude-bare-reference.sh [--file PATH]' >&2
}

validate_reference() {
  local reference="$1"
  if [ ! -f "$reference" ]; then
    printf 'expected Claude headless reference file %s, received none\n' "$reference" >&2
    return 2
  fi
  if python3 - "$reference" <<'PY'; then
import re
import sys
from html.parser import HTMLParser
from pathlib import Path


class VisibleReference(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.hidden = 0
        self.parts = []

    def handle_starttag(self, tag, attrs):
        if tag in {"script", "style", "pre"}:
            self.hidden += 1

    def handle_endtag(self, tag):
        if tag in {"script", "style", "pre"}:
            self.hidden = max(0, self.hidden - 1)

    def handle_data(self, data):
        if not self.hidden:
            self.parts.append(data)


source = Path(sys.argv[1]).read_text(encoding="utf-8")
source = re.sub(r"(?ms)^[ \t]*```[^\n]*\n.*?^[ \t]*```[ \t]*(?:\n|$)", " ", source)
visible = VisibleReference()
visible.feed(source.replace("`", ""))
visible.close()
text = " ".join(" ".join(visible.parts).split())
assertions = (
    "Without it, claude -p loads the same",
    "Without it, claude -p discovers and loads them as an interactive session would",
)
changed_default = re.search(
    r"(?:--bare|bare(?: mode)?) (?:is(?: now)?|has become) (?:the )?default\b",
    text,
    re.IGNORECASE,
)
sys.exit(0 if not changed_default and any(assertion in text for assertion in assertions) else 1)
PY
    echo 'Claude reference still describes --bare as opt-in'
    return 0
  fi

  echo 'expected the Claude reference to state that --bare is opt-in, received no matching assertion' >&2
  return 1
}

download_and_validate() {
  local reference
  local status
  reference=$(mktemp "${TMPDIR:-/tmp}/mea-claude-bare-reference.XXXXXX")
  # Capture the quoted local path while this function still owns it.
  # shellcheck disable=SC2064
  trap "rm -f -- $(printf '%q' "$reference")" RETURN
  curl --fail --silent --show-error --location "$CLAUDE_HEADLESS_REFERENCE" --output "$reference"
  if validate_reference "$reference"; then
    status=0
  else
    status=$?
  fi
  trap - RETURN
  rm -f -- "$reference"
  return "$status"
}

main() {
  if [ $# -eq 0 ]; then
    download_and_validate
    return
  fi
  if [ $# -eq 2 ] && [ "$1" = '--file' ]; then
    validate_reference "$2"
    return
  fi
  usage
  return 2
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
