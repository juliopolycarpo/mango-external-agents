#!/usr/bin/env bash
# Named fake for scripts/test-prune-rust-toolchains.sh. It keeps a toolchain list in the file named
# by FAKE_RUSTUP_STATE (one name per line) and logs every call, so nothing touches the real rustup.
#
# It answers the calls scripts/prune-rust-toolchains.sh may make:
#   toolchain install            adds FAKE_RUSTUP_PINNED when it is missing (the active toolchain)
#   show active-toolchain        prints FAKE_RUSTUP_PINNED the way rustup does, with its reason;
#                                with FAKE_RUSTUP_NO_ACTIVE set it fails the way rustup does
#   toolchain list --quiet       prints the list
#   toolchain uninstall NAME...  removes each name; a name that is not installed exits 1
#   default NAME                 logged only
# Any other call exits 64 and says what it received.
set -euo pipefail

call="$*"
printf '%s\n' "$call" >> "${FAKE_RUSTUP_LOG:?expected FAKE_RUSTUP_LOG}"
state=${FAKE_RUSTUP_STATE:?expected FAKE_RUSTUP_STATE}
pinned=${FAKE_RUSTUP_PINNED:?expected FAKE_RUSTUP_PINNED}

case "$call" in
  'toolchain install')
    grep -qxF "$pinned" "$state" || printf '%s\n' "$pinned" >> "$state"
    ;;
  'show active-toolchain')
    if [ -n "${FAKE_RUSTUP_NO_ACTIVE:-}" ]; then
      echo 'error: no active toolchain' >&2
      exit 1
    fi
    printf "%s (overridden by '/checkout/rust-toolchain.toml')\n" "$pinned"
    ;;
  'toolchain list --quiet')
    cat "$state"
    ;;
  'toolchain uninstall '*)
    shift 2
    for name in "$@"; do
      if ! grep -qxF "$name" "$state"; then
        echo "fake rustup: expected an installed toolchain to uninstall | received: $name" >&2
        exit 1
      fi
      grep -vxF "$name" "$state" > "$state.next" || true
      mv "$state.next" "$state"
    done
    ;;
  'default '*) ;;
  *)
    echo "fake rustup: expected 'toolchain install', 'show active-toolchain', 'toolchain list --quiet', 'toolchain uninstall <name>...' or 'default <name>' | received: $call" >&2
    exit 64
    ;;
esac
