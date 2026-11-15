#!/usr/bin/env bash
# Regression coverage for scripts/install-actionlint.sh: the pinned archive is the only one it
# extracts, a digest mismatch or an unknown platform refuses before anything lands in the bin
# directory, and a GitHub Actions step gets the directory on its PATH. The script runs from a
# scratch copy with its own registry, a named FakeCurl and a named FakeUname, so the test needs
# neither a network nor a particular host.
set -euo pipefail
cd "$(dirname "$0")/.."

work=$(mktemp -d "${TMPDIR:-/tmp}/install-actionlint-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
fail() { echo "$*" >&2; exit 1; }

version=$(sed -n 's/^ACTIONLINT_VERSION=//p' scripts/install-actionlint.sh)
[ -n "$version" ] || fail "expected scripts/install-actionlint.sh to set ACTIONLINT_VERSION, received none"
linux_amd64_asset="actionlint_${version}_linux_amd64.tar.gz"

# FakeArchive: a release asset whose binary only says which archive it came from.
mkdir -p "$work/archive-src"
printf '#!/usr/bin/env bash\necho fake-actionlint\n' > "$work/archive-src/actionlint"
chmod +x "$work/archive-src/actionlint"
tar -czf "$work/$linux_amd64_asset" -C "$work/archive-src" actionlint
archive_digest=$(sha256sum "$work/$linux_amd64_asset" | awk '{print $1}')

# FakeCurl: logs the URL, then either fails with FAKE_CURL_STATUS or writes FAKE_CURL_ARCHIVE to the
# --output path.
mkdir -p "$work/fakebin"
cat > "$work/fakebin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
output=''
url=''
while [ $# -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    http*) url=$1; shift ;;
    *) shift ;;
  esac
done
echo "$url" >> "$FAKE_CURL_LOG"
if [ "${FAKE_CURL_STATUS:-0}" -ne 0 ]; then
  echo "FakeCurl: failing as asked" >&2
  exit "$FAKE_CURL_STATUS"
fi
cp "$FAKE_CURL_ARCHIVE" "$output"
FAKE_CURL
# FakeUname: answers -s and -m from FAKE_UNAME_SYSTEM and FAKE_UNAME_MACHINE.
cat > "$work/fakebin/uname" <<'FAKE_UNAME'
#!/usr/bin/env bash
case "${1-}" in
  -s) echo "${FAKE_UNAME_SYSTEM:-Linux}" ;;
  -m) echo "${FAKE_UNAME_MACHINE:-x86_64}" ;;
  *) echo "FakeUname: expected -s or -m, received ${1-}" >&2; exit 2 ;;
esac
FAKE_UNAME
# RecordingTar: logs each call, then runs the real tar, so a case can tell whether the archive was
# opened at all.
real_tar=$(command -v tar)
printf '#!/usr/bin/env bash\necho "$*" >> "$RECORDING_TAR_LOG"\nexec %q "$@"\n' "$real_tar" > "$work/fakebin/tar"
chmod +x "$work/fakebin/curl" "$work/fakebin/uname" "$work/fakebin/tar"

# make_copy <name> <digest>: a scratch scripts/ directory holding the script and a registry that
# pins <digest> for the Linux x86_64 asset only.
make_copy() {
  mkdir -p "$work/$1"
  cp scripts/install-actionlint.sh "$work/$1/"
  printf '%s %s %s\n' "$version" "$linux_amd64_asset" "$2" > "$work/$1/actionlint-pinned-sha256sums.txt"
}
make_copy good "$archive_digest"
make_copy wrong "$(printf '0%.0s' $(seq 64))"

# run_install <copy> [args...]: sets status, stdout_text, stderr_text. The caller's GITHUB_PATH is
# dropped, since CI sets a real one; STEP_GITHUB_PATH stands in for it.
run_install() {
  local copy=$1
  shift
  local step_env=()
  if [ -n "${STEP_GITHUB_PATH-}" ]; then step_env=(GITHUB_PATH="$STEP_GITHUB_PATH"); fi
  status=0
  : > "$work/curl.log"
  : > "$work/tar.log"
  env -u GITHUB_PATH "${step_env[@]}" PATH="$work/fakebin:$PATH" FAKE_CURL_LOG="$work/curl.log" \
    RECORDING_TAR_LOG="$work/tar.log" \
    FAKE_CURL_ARCHIVE="$work/$linux_amd64_asset" \
    "$BASH" "$work/$copy/install-actionlint.sh" "$@" > "$work/stdout" 2> "$work/stderr" || status=$?
  stdout_text=$(cat "$work/stdout")
  stderr_text=$(cat "$work/stderr")
}

# Case 1: the pinned archive installs an executable binary and, on a GitHub Actions step, the
# directory goes on the step PATH file.
: > "$work/github-path"
STEP_GITHUB_PATH="$work/github-path" run_install good "$work/bin-ok"
[ "$status" -eq 0 ] || fail "expected the install to pass for the pinned archive | received: exit $status: $stderr_text"
[ -x "$work/bin-ok/actionlint" ] || fail "expected an executable $work/bin-ok/actionlint | received none"
[ "$("$work/bin-ok/actionlint")" = fake-actionlint ] || fail "expected the installed binary to be the archive's actionlint"
expected_url="https://github.com/rhysd/actionlint/releases/download/v$version/$linux_amd64_asset"
[ "$(cat "$work/curl.log")" = "$expected_url" ] || fail "expected one download from $expected_url | received: $(cat "$work/curl.log")"
grep -q "installed actionlint $version at $work/bin-ok/actionlint" <<<"$stdout_text" || fail "expected the install to report the version and path | received: $stdout_text"
[ "$(cat "$work/github-path")" = "$work/bin-ok" ] || fail "expected GITHUB_PATH to receive $work/bin-ok | received: $(cat "$work/github-path")"
[ -s "$work/tar.log" ] || fail "expected the verified archive to be extracted with tar | received no tar call"

# Case 2: without GITHUB_PATH nothing else is written.
: > "$work/github-path"
run_install good "$work/bin-plain"
[ "$status" -eq 0 ] || fail "expected the install to pass without GITHUB_PATH | received: exit $status: $stderr_text"
[ ! -s "$work/github-path" ] || fail "expected no write to the GITHUB_PATH file when the variable is unset | received: $(cat "$work/github-path")"

# Case 3: a digest mismatch refuses before extraction and names both digests.
run_install wrong "$work/bin-wrong"
[ "$status" -eq 1 ] || fail "expected exit 1 for a digest mismatch | received: exit $status: $stderr_text"
grep -q "expected SHA-256 0000.* received $archive_digest" <<<"$stderr_text" || fail "expected the mismatch to name the pinned and received digests | received: $stderr_text"
[ ! -e "$work/bin-wrong/actionlint" ] || fail "expected no binary after a digest mismatch | received $work/bin-wrong/actionlint"
[ ! -s "$work/tar.log" ] || fail "expected no tar call on an archive whose digest does not match | received: $(cat "$work/tar.log")"

# Case 4: an unsupported platform is named and nothing is downloaded.
FAKE_UNAME_SYSTEM=FreeBSD run_install good "$work/bin-bsd"
[ "$status" -eq 2 ] || fail "expected exit 2 for FreeBSD | received: exit $status: $stderr_text"
grep -q 'received system FreeBSD and architecture x86_64' <<<"$stderr_text" || fail "expected the platform in the message | received: $stderr_text"
[ ! -s "$work/curl.log" ] || fail "expected no download for an unsupported platform | received: $(cat "$work/curl.log")"

# Case 5: a supported platform missing from the registry is named and nothing is downloaded.
FAKE_UNAME_SYSTEM=Darwin FAKE_UNAME_MACHINE=arm64 run_install good "$work/bin-darwin"
[ "$status" -eq 2 ] || fail "expected exit 2 for an asset the registry lacks | received: exit $status: $stderr_text"
grep -q "expected a pinned SHA-256 for actionlint $version asset actionlint_${version}_darwin_arm64.tar.gz" <<<"$stderr_text" || fail "expected the missing asset in the message | received: $stderr_text"
[ ! -s "$work/curl.log" ] || fail "expected no download for an unpinned asset | received: $(cat "$work/curl.log")"

# Case 6: a failed download leaves no binary.
FAKE_CURL_STATUS=22 run_install good "$work/bin-offline"
[ "$status" -ne 0 ] || fail "expected a failed download to fail the install | received: exit 0"
[ ! -e "$work/bin-offline/actionlint" ] || fail "expected no binary after a failed download | received $work/bin-offline/actionlint"

# Case 7: no argument prints the usage and exits 2.
run_install good
[ "$status" -eq 2 ] || fail "expected exit 2 without a bin directory | received: exit $status"
grep -q 'usage: scripts/install-actionlint.sh <bin-dir>' <<<"$stderr_text" || fail "expected the usage line | received: $stderr_text"

# Case 8: the committed registry pins every platform the script maps, for the version it installs.
for platform in darwin_amd64 darwin_arm64 linux_amd64 linux_arm64; do
  asset="actionlint_${version}_${platform}.tar.gz"
  line=$(grep -E "^$version $asset [0-9a-f]{64}$" scripts/actionlint-pinned-sha256sums.txt || true)
  [ -n "$line" ] || fail "expected scripts/actionlint-pinned-sha256sums.txt to pin $asset at $version with a 64-digit hex SHA-256 | received none"
done

echo "install-actionlint: all cases passed"
