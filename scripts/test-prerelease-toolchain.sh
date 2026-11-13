#!/usr/bin/env bash
# Check that the package probe preserves the checkout's selected toolchain after changing directory.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
scratch=$(mktemp -d "${TMPDIR:-/tmp}/mango-prerelease-fake.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/package/mango-external-agents-1.2.3"
printf '[package]\nname = "mango-external-agents"\nversion = "1.2.3"\n' > "$scratch/package/mango-external-agents-1.2.3/Cargo.toml"
cat > "$scratch/bin/rustup" <<'FAKE_RUSTUP'
#!/usr/bin/env bash
set -euo pipefail
[ "$PWD" = "$FAKE_REPO" ] && [ "$*" = 'show active-toolchain' ] || exit 1
printf 'checkout-toolchain (overridden by checkout)\n'
FAKE_RUSTUP
cat > "$scratch/bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail
if [ "$1" != package ] && [ "${RUSTUP_TOOLCHAIN:-}" != checkout-toolchain ]; then
  echo "expected toolchain checkout-toolchain, received ${RUSTUP_TOOLCHAIN:-unset} in $PWD" >&2
  exit 1
fi
printf '%s %s\n' "$PWD" "$*" >> "$FAKE_LOG"
case "$1" in
  package)
    mkdir -p "$CARGO_TARGET_DIR/package"
    tar -czf "$CARGO_TARGET_DIR/package/mango-external-agents-1.2.3.crate" -C "$FAKE_SCRATCH/package" mango-external-agents-1.2.3
    ;;
  generate-lockfile|nextest)
    [ "$PWD" != "$FAKE_REPO" ] || { echo 'expected extracted package directory, received checkout' >&2; exit 1; }
    grep -qx 'version = "1.2.3-rc.1"' Cargo.toml || { echo 'expected prerelease 1.2.3-rc.1, received another manifest' >&2; exit 1; }
    ;;
  *) echo "expected package, generate-lockfile or nextest, received $*" >&2; exit 1 ;;
esac
FAKE_CARGO
chmod +x "$scratch/bin/rustup" "$scratch/bin/cargo"
export FAKE_REPO="$repo" FAKE_SCRATCH="$scratch" FAKE_LOG="$scratch/calls"
PATH="$scratch/bin:$PATH" env -u RUSTUP_TOOLCHAIN scripts/test-prerelease-package.sh
if [ "$(wc -l < "$FAKE_LOG")" -ne 3 ]; then
  echo "expected three cargo calls, received $(cat "$FAKE_LOG")" >&2
  exit 1
fi
echo 'prerelease toolchain selection passed'
