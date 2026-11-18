#!/usr/bin/env bash
# What the two ACP watches share: the pattern that says a text names ACP, and the byte cut that keeps
# a reported line valid UTF-8.
#
# `scripts/check-vendor-acp-docs.sh` reads vendor documentation pages and
# `scripts/check-vendor-subcommand-help.sh` reads each subcommand's help. Both must mean the same
# thing by "names ACP", and a rule written twice can disagree with itself; this file is the one copy.
# It is sourced, never run, and sourcing it twice is harmless because `scripts/test-vendor-drift.sh`
# sources scripts that each source it.

if [ -z "${VENDOR_ACP_LIB_LOADED:-}" ]; then
  VENDOR_ACP_LIB_LOADED=1

  # Case-insensitive; `\b` keeps a hash or a longer word that merely contains "acp" from matching,
  # while `--acp`, `acp` and `claude-agent-acp` do match.
  readonly ACP_PATTERN='\bacp\b|agent[ -]?client[ -]?protocol'

  # Cuts each line of stdin at a byte limit (the first argument, default 1000) and drops a UTF-8
  # character the cut split, so the report never carries a lone lead byte (an issue body with one is
  # not valid UTF-8). The cut keeps the first bytes, so the dropped tail is the lead byte and the
  # continuation bytes before the cut: `[C2-DF]`, `[E0-EF]` plus at most one `[80-BF]`, or `[F0-F4]`
  # plus at most two. A complete character has all its bytes and does not match. GNU sed reads
  # `\xHH` in a bracket expression.
  cut_lines_to_report_bytes() {
    local limit="${1:-1000}"
    LC_ALL=C cut -c "1-$limit" \
      | LC_ALL=C sed -E 's/([\xc2-\xdf]|[\xe0-\xef][\x80-\xbf]?|[\xf0-\xf4][\x80-\xbf]{0,2})$//'
  }
fi
