#!/usr/bin/env bash
# Named fake for the vendor documentation fetcher: serves a page from $FAKE_DOC_ROOT instead of the
# network. The file for a URL is the URL with every character outside [A-Za-z0-9] replaced by `_`.
# A URL with no file answers like `curl --fail` on an HTTP error (exit 22).
set -euo pipefail

url=''
output=''
while [ $# -gt 0 ]; do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    --fail | --silent | --show-error | --location) shift ;;
    -*) printf 'expected a curl flag the doc watch sends, received %s\n' "$1" >&2; exit 2 ;;
    *) url="$1"; shift ;;
  esac
done

if [ -z "$url" ] || [ -z "$output" ]; then
  printf 'expected a URL and --output, received url=%s output=%s\n' "$url" "$output" >&2
  exit 2
fi

page="${FAKE_DOC_ROOT:?expected FAKE_DOC_ROOT}/$(printf '%s' "$url" | tr -c 'A-Za-z0-9' '_')"
if [ ! -f "$page" ]; then
  printf 'fake curl: (22) no page for %s\n' "$url" >&2
  exit 22
fi
cp -- "$page" "$output"
