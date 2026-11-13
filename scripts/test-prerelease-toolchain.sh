#!/usr/bin/env bash
# Check that the package probe preserves the checkout's selected toolchain after changing directory.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=$PWD
scratch=$(mktemp -d "${TMPDIR:-/tmp}/mango-prerelease-fake.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/package/mango-external-agents-1.2.3"
printf '[dependencies.example]\nversion = "4.5.6"\n\n[package]\nname = "mango-external-agents"\nversion = "1.2.3"\n' > "$scratch/package/mango-external-agents-1.2.3/Cargo.toml"
printf 'core = "1.2.3"\ndependency = "1.0.151"\n' > "$scratch/package/mango-external-agents-1.2.3/Cargo.lock"
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
  update|nextest)
    [ "$PWD" != "$FAKE_REPO" ] || { echo 'expected extracted package directory, received checkout' >&2; exit 1; }
    python3 - <<'CHECK_MANIFEST'
import tomllib
from pathlib import Path
manifest = tomllib.loads(Path('Cargo.toml').read_text())
for label, actual, expected in (
    ('package.version', manifest['package']['version'], '1.2.3-rc.1'),
    ('dependencies.example.version', manifest['dependencies']['example']['version'], '4.5.6'),
):
    if actual != expected:
        raise SystemExit(f'{label}: expected {expected}, received {actual}')
CHECK_MANIFEST
    if [ "$1" = update ]; then
      [ "$*" = 'update --workspace --offline' ] || { echo "expected cargo update --workspace --offline, received cargo $*" >&2; exit 1; }
      sed -i 's/core = "1.2.3"/core = "1.2.3-rc.1"/' Cargo.lock
    else
      [ "$*" = 'nextest run --lib --all-features --locked' ] || { echo "expected cargo nextest run --lib --all-features --locked, received cargo $*" >&2; exit 1; }
      grep -qx 'core = "1.2.3-rc.1"' Cargo.lock || { echo 'expected updated core version in Cargo.lock' >&2; exit 1; }
      grep -qx 'dependency = "1.0.151"' Cargo.lock || { echo 'expected dependency 1.0.151 preserved in Cargo.lock' >&2; exit 1; }
    fi
    ;;
  *) echo "expected cargo update --workspace --offline or locked nextest, received cargo $*" >&2; exit 1 ;;
esac
FAKE_CARGO
chmod +x "$scratch/bin/rustup" "$scratch/bin/cargo"
export FAKE_REPO="$repo" FAKE_SCRATCH="$scratch" FAKE_LOG="$scratch/calls"
PATH="$scratch/bin:$PATH" env -u RUSTUP_TOOLCHAIN scripts/test-prerelease-package.sh
if [ "$(wc -l < "$FAKE_LOG")" -ne 3 ]; then
  echo "expected three cargo calls, received $(cat "$FAKE_LOG")" >&2
  exit 1
fi
for case_name in missing repeated; do
  manifest="$scratch/package/mango-external-agents-1.2.3/Cargo.toml"
  if [ "$case_name" = missing ]; then
    sed -i '/^version = "1.2.3"$/d' "$manifest"
    expected_count=0
  else
    printf 'version = "1.2.3"\nversion = "1.2.4"\n' >> "$manifest"
    expected_count=2
  fi
  if PATH="$scratch/bin:$PATH" env -u RUSTUP_TOOLCHAIN scripts/test-prerelease-package.sh > "$scratch/$case_name.log" 2>&1; then
    echo "$case_name package version: expected probe failure, received success" >&2
    exit 1
  fi
  if ! grep -Fq "expected one package version assignment, received $expected_count" "$scratch/$case_name.log"; then
    echo "$case_name package version: expected assignment count $expected_count, received $(cat "$scratch/$case_name.log")" >&2
    exit 1
  fi
done
echo 'prerelease toolchain selection passed'
