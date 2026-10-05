#!/usr/bin/env bash
# Regression coverage for tag-only dispatch and mismatched release versions.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(scripts/release-version.sh refs/tags/v0.1.0)" == '0.1.0' ]]
[[ "$(scripts/release-version.sh refs/tags/v0.2.0-rc.1 0.2.0-rc.1)" == '0.2.0-rc.1' ]]
for ref in refs/heads/main refs/heads/v0.1.0 refs/tags/v0.1 refs/tags/v0.1.0-canary.1; do
  if scripts/release-version.sh "$ref" 0.1.0; then
    echo "expected rejection of non-release ref, received success for $ref" >&2
    exit 1
  fi
done
if scripts/release-version.sh refs/tags/v0.1.0 0.2.0; then
  echo 'expected rejection of mismatched version, received success' >&2
  exit 1
fi
echo 'release tag checks passed'

# A pull request title becomes the squash subject, and git-cliff reads that subject. The rejected
# case below is verbatim the title that shipped v0.1.0's release notes without the pull request
# that produced the tagged tree.
for title in 'feat(core): add a thing' 'fix: no scope' 'docs(release): record the bootstrap' \
  'feat(acp)!: breaking change' 'chore(deps): bump a dependency' \
  'migration(fixtures): a type only cliff.toml names'; do
  if ! scripts/check-pr-title.sh "$title" >/dev/null; then
    echo "expected acceptance of '$title', received the rejection above" >&2
    exit 1
  fi
done
while IFS= read -r title; do
  if scripts/check-pr-title.sh "$title" >/dev/null 2>&1; then
    echo "expected rejection of '$title', received success" >&2
    exit 1
  fi
done <<'TITLES'
Release gate: host adoption, Hub-owned retry, and publication readiness (#18)
feat(nope): a scope AGENTS.md does not name
nope(core): a type the changelog cannot group
feat(core):no space after the colon
feat(core):
feat(): an empty scope is not a scope
TITLES
# Separate from the list above because a tab cannot survive being read back as literal text. The
# space before the tab is load-bearing: `[[:space:]]` in the subject pattern consumes one character,
# so `feat(core):<tab>` is rejected by the pattern itself and would pass this test against the
# whitespace guard it is meant to exercise. With the space, the summary is the tab alone.
tab_title=$(printf 'feat(core): \t')
if rejection=$(scripts/check-pr-title.sh "$tab_title" 2>&1); then
  echo 'expected rejection of a summary that is only a tab, received success' >&2
  exit 1
fi
case "$rejection" in
  *'received only whitespace'*) ;;
  *)
    echo 'expected the whitespace diagnostic for a tab-only summary, received:' >&2
    echo "$rejection" >&2
    exit 1
    ;;
esac
if scripts/check-pr-title.sh >/dev/null 2>&1; then
  echo 'expected rejection of an empty title, received success' >&2
  exit 1
fi
echo 'pull request title checks passed'

# The release tag must be an annotated tag GitHub verified. The fake `gh` answers for one tag; every
# refusal has to name the tag and what GitHub reported, so a maintainer can act on the log alone.
tag_bin=$(mktemp -d)
tag_log=$tag_bin/gh.log
ln -s "$PWD/scripts/test-fixtures/fake-tag-verification-cli.sh" "$tag_bin/gh"
check_tag() {
  env PATH="$tag_bin:$PATH" FAKE_GH_LOG=$tag_log FAKE_TAG_NAME=v0.4.0 "$@" \
    scripts/check-release-tag-signature.sh owner/repository "$TAG_VERSION" "$RELEASE_COMMIT"
}
expect_tag_refusal() {
  local status=$1
  local expected=$2
  shift 2
  local output
  local received
  set +e
  output=$(check_tag "$@" 2>&1)
  received=$?
  set -e
  if [ "$received" != "$status" ]; then
    echo "expected exit $status from the tag check, received $received: $output" >&2
    exit 1
  fi
  case "$output" in
    *"$expected"*) ;;
    *)
      echo "expected the tag check to say '$expected', received: $output" >&2
      exit 1
      ;;
  esac
}
TAG_VERSION=0.4.0
RELEASE_COMMIT=56307d67182e53594382e04682800b36eb421689
check_tag FAKE_TAG_VERIFIED=true FAKE_TAG_REASON=valid >/dev/null || {
  echo 'expected a verified annotated tag to pass, received a refusal' >&2
  exit 1
}
expect_tag_refusal 1 'expected tag v0.4.0 to be signed and verified by GitHub, received verified=false reason=unsigned' \
  FAKE_TAG_VERIFIED=false FAKE_TAG_REASON=unsigned
expect_tag_refusal 1 'received verified=false reason=unknown_key' \
  FAKE_TAG_VERIFIED=false FAKE_TAG_REASON=unknown_key
expect_tag_refusal 1 'expected v0.4.0 to be an annotated signed tag, received a commit object' \
  FAKE_TAG_OBJECT=commit
expect_tag_refusal 1 'expected tag v0.4.0 to be signed under that name, received a signature over the name candidate' \
  FAKE_TAG_SIGNED_NAME=candidate
expect_tag_refusal 1 'expected tag v0.4.0 to point at the commit being released 56307d67182e53594382e04682800b36eb421689, received 1111111111111111111111111111111111111111' \
  FAKE_TAG_COMMIT=1111111111111111111111111111111111111111
TAG_VERSION=0.9.0
expect_tag_refusal 2 'expected tag v0.9.0 to exist in owner/repository' FAKE_TAG_VERIFIED=true
if scripts/check-release-tag-signature.sh owner/repository 0.4.0 >/dev/null 2>&1; then
  echo 'expected the tag check to refuse a missing version, received success' >&2
  exit 1
fi
rm -rf "$tag_bin"
echo 'release tag signature checks passed'

# A tag must pass the isolated minimum-Rust feature check before the publish job exists to run, and
# a release workflow change can only be exercised by a real tag, so its shape is pinned here. The
# verify job installs the toolchain the script itself names (no version literal to drift from
# `rust-version`), runs `scripts/check-msrv-features.sh --locked`, and never selects the minimum for
# the other steps: `check.sh` and `check-publish.sh` stay on the pinned toolchain. Comment lines are
# ignored so a note cannot satisfy a check. Every refusal names the behaviour, the file and what
# the file holds instead. RELEASE_WORKFLOW points the check at another file.
release_workflow=${RELEASE_WORKFLOW:-$PWD/.github/workflows/release.yml}
workflow_job() {
  awk -v job="  $2:" '$0 == job { found = 1; next } found && /^  [A-Za-z0-9_-]+:/ { found = 0 } found' "$1" |
    grep -v '^[[:space:]]*#' || true
}
check_release_workflow() {
  local file=$1
  local verify check_line install_line found
  verify=$(workflow_job "$file" verify)
  if ! grep -qF -- 'scripts/check-msrv-features.sh --locked' <<<"$verify"; then
    echo "expected the release verify job in $file to run the isolated minimum-Rust feature check 'scripts/check-msrv-features.sh --locked' before publishing, received no such step" >&2
    return 1
  fi
  if ! grep -qF -- 'rustup toolchain install' <<<"$verify" || ! grep -qF -- 'check-msrv-features.sh --print-toolchain' <<<"$verify"; then
    echo "expected the release verify job in $file to install the minimum toolchain named by 'scripts/check-msrv-features.sh --print-toolchain', received no such install step" >&2
    return 1
  fi
  install_line=$(grep -nF -- 'rustup toolchain install' <<<"$verify" | head -n 1)
  if grep -q '[0-9]\.[0-9]' <<<"$install_line"; then
    echo "expected the minimum toolchain install in $file to read its version from the script, received a literal version: ${install_line#*:}" >&2
    return 1
  fi
  check_line=$(grep -nF -- 'scripts/check-msrv-features.sh --locked' <<<"$verify" | head -n 1)
  if [ "${install_line%%:*}" -ge "${check_line%%:*}" ]; then
    echo "expected the minimum toolchain to be installed before the isolated feature check in $file, received the install at verify line ${install_line%%:*} and the check at line ${check_line%%:*}" >&2
    return 1
  fi
  found=$(workflow_job "$file" crates | grep -E '^[[:space:]]+needs:[[:space:]]*verify[[:space:]]*$' || true)
  if [ -z "$found" ]; then
    echo "expected the publish job (crates) in $file to need the verify job, received no 'needs: verify'" >&2
    return 1
  fi
  found=$(grep -v '^[[:space:]]*#' "$file" | grep -E 'RUSTUP_TOOLCHAIN.*GITHUB_ENV|^[[:space:]]*RUSTUP_TOOLCHAIN:' || true)
  if [ -n "$found" ]; then
    echo "expected $file to leave the pinned toolchain selected for every other step, received RUSTUP_TOOLCHAIN set job-wide in: $found" >&2
    return 1
  fi
}
check_release_workflow "$release_workflow"
workflow_dir=$(mktemp -d)
trap 'rm -rf "$workflow_dir"' EXIT
expect_workflow_refusal() {
  local expected=$1
  local output
  if output=$(check_release_workflow "$workflow_dir/release.yml" 2>&1); then
    echo "expected the workflow check to refuse a mutated copy with '$expected', received success" >&2
    exit 1
  fi
  case "$output" in
    *"$expected"*) ;;
    *)
      echo "expected the workflow check to say '$expected', received: $output" >&2
      exit 1
      ;;
  esac
}
grep -v 'check-msrv-features.sh --locked' "$release_workflow" > "$workflow_dir/release.yml"
expect_workflow_refusal 'to run the isolated minimum-Rust feature check'
grep -v 'rustup toolchain install' "$release_workflow" > "$workflow_dir/release.yml"
expect_workflow_refusal 'to install the minimum toolchain named by'
sed 's/rustup toolchain install "/rustup toolchain install 1.97.0 "/' "$release_workflow" > "$workflow_dir/release.yml"
expect_workflow_refusal 'to read its version from the script, received a literal version'
awk '/rustup toolchain install/ { held = $0; next } { print } /scripts\/check-msrv-features.sh --locked/ { print held }' \
  "$release_workflow" > "$workflow_dir/release.yml"
expect_workflow_refusal 'to be installed before the isolated feature check'
sed 's/needs: verify$/needs: []/' "$release_workflow" > "$workflow_dir/release.yml"
expect_workflow_refusal 'to need the verify job'
{ cat "$release_workflow"; printf '      - run: echo "RUSTUP_TOOLCHAIN=1.97.0" >> "$GITHUB_ENV"\n'; } > "$workflow_dir/release.yml"
expect_workflow_refusal 'to leave the pinned toolchain selected for every other step'
rm -rf "$workflow_dir"
trap - EXIT
echo 'release workflow checks passed'

# What the title gate protects: git-cliff has to keep a squash subject that carries no Conventional
# Commit type, show only its first line, and mark a breaking change. `filter_unconventional = true`
# — the setting that kept #18 out of v0.1.0's notes — fails the first assertion, dropping the
# `split` filter fails the second, and dropping the `commit.breaking` branch fails the third.
if ! command -v git-cliff >/dev/null 2>&1; then
  echo 'skipping the changelog regression: git-cliff is not on PATH' >&2
else
  config=$PWD/cliff.toml
  fixture=$(mktemp -d)
  trap 'rm -rf "$fixture"' EXIT
  commit() {
    git -C "$fixture" -c user.name=fixture -c user.email=fixture@example.invalid \
      -c commit.gpgsign=false commit -q --allow-empty "$@"
  }
  git init -q -b main "$fixture"
  commit -m 'feat(core)!: a breaking change' -m 'A body.'
  commit -m 'Release gate: a subject with no type (#18)' \
    -m 'A body line the changelog must not repeat.'
  notes=$(git-cliff --config "$config" --repository "$fixture" 2>/dev/null)
  for expected in '- Release gate: a subject with no type (#18)' \
    '- [**breaking**] **(core)** A breaking change'; do
    case "$notes" in
      *"$expected"*) ;;
      *)
        echo "expected the changelog to contain '$expected', received:" >&2
        echo "$notes" >&2
        exit 1
        ;;
    esac
  done
  case "$notes" in
    *'A body line the changelog must not repeat.'*)
      echo 'expected the changelog to carry subjects only, received a commit body:' >&2
      echo "$notes" >&2
      exit 1
      ;;
  esac
  rm -rf "$fixture"
  trap - EXIT
  echo 'changelog rendering checks passed'
fi

# A full rerun after the release exists must not fail at the last step, and a first run must still
# create it. The fake `gh` refuses to create a release that exists, and records what it was asked.
release_bin=$(mktemp -d)
release_log=$release_bin/gh.log
trap 'rm -rf "$release_bin"' EXIT
ln -s "$PWD/scripts/test-fixtures/fake-release-publish-cli.sh" "$release_bin/gh"
printf 'notes\n' > "$release_bin/notes.md"
publish() {
  : > "$release_log"
  PATH="$release_bin:$PATH" FAKE_GH_LOG=$release_log FAKE_GH_EXISTING_RELEASE=$1 \
    scripts/publish-github-release.sh "$2" "$release_bin/notes.md" >/dev/null 2>&1
}
expect_release_calls() {
  local expected=$1
  local received
  received=$(tr '\n' '|' < "$release_log")
  if [ "$received" != "$expected" ]; then
    echo "expected gh calls '$expected', received '$received'" >&2
    exit 1
  fi
}
publish v0.4.0 0.4.0 || {
  echo 'expected a rerun to succeed when release v0.4.0 exists, received a failure' >&2
  exit 1
}
expect_release_calls 'release view v0.4.0|'
publish none 0.4.0 || {
  echo 'expected the first run to create release v0.4.0, received a failure' >&2
  exit 1
}
expect_release_calls "release view v0.4.0|release create v0.4.0 --title v0.4.0 --notes-file $release_bin/notes.md|"
publish none 0.5.0-rc.1 || {
  echo 'expected the first run to create prerelease v0.5.0-rc.1, received a failure' >&2
  exit 1
}
expect_release_calls "release view v0.5.0-rc.1|release create v0.5.0-rc.1 --title v0.5.0-rc.1 --notes-file $release_bin/notes.md --prerelease|"
if PATH="$release_bin:$PATH" FAKE_GH_LOG=$release_log scripts/publish-github-release.sh 0.4.0 "$release_bin/missing.md" 2>/dev/null; then
  echo 'expected a missing notes file to be refused, received success' >&2
  exit 1
fi
rm -rf "$release_bin"
trap - EXIT
echo 'github release checks passed'

# A path-only dev-dependency vanishes from the packaged manifest, taking its features with it.
# `workspace = true` and a version beside the path both survive; a bare path does not.
dev_fixture=$(mktemp -d)
trap 'rm -rf "$dev_fixture"' EXIT
printf '[dev-dependencies]\n# path = "../commented" is only a comment\nsibling = { workspace = true, features = ["testing"] }\nother = { path = "../other", version = "0.3.0" }\n' > "$dev_fixture/kept.toml"
printf '[dev-dependencies]\nsibling = { features = ["testing"], path = "../sibling" }\n' > "$dev_fixture/dropped.toml"
printf 'dev-dependencies.sibling.path = "../sibling"\n\n[package]\nname = "x"\n' > "$dev_fixture/dotted.toml"
printf '[target.x86_64-unknown-linux-gnu.dev-dependencies]\nsibling = { path = "../sibling" }\n' > "$dev_fixture/target.toml"
scripts/check-dev-dependencies.sh "$dev_fixture/kept.toml" >/dev/null
for rejected in dropped dotted target; do
  if scripts/check-dev-dependencies.sh "$dev_fixture/$rejected.toml" >/dev/null 2>"$dev_fixture/$rejected.err"; then
    echo "expected rejection of the path-only dev-dependency in $rejected.toml, received success" >&2
    exit 1
  fi
  grep -q "$rejected.toml: \[.*dev-dependencies\] sibling = " "$dev_fixture/$rejected.err" || {
    echo "expected the rejection to name $rejected.toml and the dependency, received: $(cat "$dev_fixture/$rejected.err")" >&2
    exit 1
  }
done
rm -rf "$dev_fixture"
trap - EXIT
scripts/check-dev-dependencies.sh >/dev/null
echo 'dev-dependency packaging checks passed'

# A crate README's install requirement follows the release rule: `major.minor` for a release, the
# full version for a pre-release. 0.3.0 shipped "0.1", which no 0.3 release satisfies. Every
# refusal has to name the README, the value it carries and the value it should carry.
readme_fixture=$(mktemp -d)
trap 'rm -rf "$readme_fixture"' EXIT
write_readme() {
  printf '# crate\n\n```toml\n[dependencies]\nmango-external-agents = "%s"\nmango-agent-acp = "%s"\n```\n' "$2" "$3" > "$readme_fixture/$1.md"
}
expect_readme_refusal() {
  local version=$1
  local expected=$2
  shift 2
  local output
  if output=$(scripts/check-readme-requirements.sh "$version" "$@" 2>&1); then
    echo "expected the README check to refuse, received success for $version: $*" >&2
    exit 1
  fi
  case "$output" in
    *"$expected"*) ;;
    *)
      echo "expected the README check to say '$expected', received: $output" >&2
      exit 1
      ;;
  esac
}
write_readme current 0.3 0.3
write_readme stale 0.3 0.1
write_readme patch 0.3.1 0.3
write_readme prerelease 0.4.0-rc.1 0.4.0-rc.1
write_readme caret 0.4 0.4
printf '# crate\n\nNo install snippet here.\n' > "$readme_fixture/none.md"
scripts/check-readme-requirements.sh 0.3.1 "$readme_fixture/current.md" >/dev/null || {
  echo 'expected "0.3" to satisfy workspace version 0.3.1, received a refusal' >&2
  exit 1
}
scripts/check-readme-requirements.sh 0.4.0-rc.1 "$readme_fixture/prerelease.md" >/dev/null || {
  echo 'expected the full version to satisfy pre-release 0.4.0-rc.1, received a refusal' >&2
  exit 1
}
expect_readme_refusal 0.3.1 "$readme_fixture/stale.md:6: expected mango-agent-acp = \"0.3\" for workspace version 0.3.1, received \"0.1\"" \
  "$readme_fixture/stale.md"
expect_readme_refusal 0.3.1 "$readme_fixture/patch.md:5: expected mango-external-agents = \"0.3\" for workspace version 0.3.1, received \"0.3.1\"" \
  "$readme_fixture/patch.md"
expect_readme_refusal 0.4.0-rc.1 "$readme_fixture/caret.md:5: expected mango-external-agents = \"0.4.0-rc.1\" for workspace version 0.4.0-rc.1, received \"0.4\"" \
  "$readme_fixture/caret.md"
# Inline tables and trailing comments are valid TOML, so a stale one must not hide behind the simple
# line that sits beside it; a form the check cannot read is refused rather than skipped.
printf 'mango-external-agents = "0.3"\nmango-agent-acp = { version = "0.1", features = ["testing"] }\n' > "$readme_fixture/table-stale.md"
printf 'mango-external-agents = "0.3" # core\nmango-agent-acp = { features = ["testing"], version = "0.3" } # harness\n' > "$readme_fixture/table-current.md"
printf 'mango-external-agents = "0.3"\nmango-agent-acp = "0.1" # stale\n' > "$readme_fixture/comment-stale.md"
printf 'mango-external-agents = "0.3"\nmango-agent-acp = { path = "../acp" }\n' > "$readme_fixture/table-path.md"
scripts/check-readme-requirements.sh 0.3.1 "$readme_fixture/table-current.md" >/dev/null || {
  echo 'expected an inline table and a trailing comment at "0.3" to pass, received a refusal' >&2
  exit 1
}
expect_readme_refusal 0.3.1 "$readme_fixture/table-stale.md:2: expected mango-agent-acp = \"0.3\" for workspace version 0.3.1, received \"0.1\"" \
  "$readme_fixture/table-stale.md"
expect_readme_refusal 0.3.1 "$readme_fixture/comment-stale.md:2: expected mango-agent-acp = \"0.3\" for workspace version 0.3.1, received \"0.1\"" \
  "$readme_fixture/comment-stale.md"
expect_readme_refusal 0.3.1 "$readme_fixture/table-path.md:2: expected mango-agent-acp = \"0.3\" or an inline table with version = \"0.3\", received unsupported form: { path = \"../acp\" }" \
  "$readme_fixture/table-path.md"
expect_readme_refusal 0.3.1 "$readme_fixture/none.md: expected a 'mango-… = \"0.3\"' install requirement, received none" \
  "$readme_fixture/none.md"
expect_readme_refusal 0.3.1 "$readme_fixture/missing.md: expected a README file, received none" \
  "$readme_fixture/missing.md"
expect_readme_refusal not-a-version "expected a version like 0.3.1 or 0.4.0-rc.1, received 'not-a-version'" \
  "$readme_fixture/current.md"

# `check-versions.sh` is what `check.sh`, CI and the release workflow run, so the README rule has to
# be wired into it: a workspace in lockstep at 0.3.1 whose README says "0.1" must not pass.
mkdir -p "$readme_fixture/tree/scripts" "$readme_fixture/tree/crates/demo" "$readme_fixture/tree/examples/demo-host"
cp scripts/check-versions.sh scripts/check-readme-requirements.sh scripts/check-consumer-pin.sh "$readme_fixture/tree/scripts/"
mkdir -p "$readme_fixture/tree/tests/standalone-current"
printf '[dependencies]\nmango-demo = { version = "=0.3.1", default-features = false }\n' > "$readme_fixture/tree/tests/standalone-current/Cargo.toml"
printf '[workspace.package]\nversion = "0.3.1"\n\n[workspace.dependencies]\nmango-demo = { path = "crates/demo", version = "0.3.1" }\n' > "$readme_fixture/tree/Cargo.toml"
printf '[package]\nname = "mango-demo"\nversion.workspace = true\n' > "$readme_fixture/tree/crates/demo/Cargo.toml"
printf '[package]\nname = "demo-host"\nversion.workspace = true\n' > "$readme_fixture/tree/examples/demo-host/Cargo.toml"
write_readme tree/crates/demo/README 0.3 0.3
if ! "$readme_fixture/tree/scripts/check-versions.sh" >/dev/null 2>&1; then
  echo 'expected a lockstep workspace with current README snippets to pass, received a refusal' >&2
  exit 1
fi
# The registry consumer's pin is part of the same gate: one release behind passes (a release pull
# request precedes its publication), two behind is refused with both values named.
set_consumer_pin() {
  printf '[dependencies]\nmango-demo = { version = "=%s", default-features = false }\n' "$1" > "$readme_fixture/tree/tests/standalone-current/Cargo.toml"
}
set_workspace_version() {
  printf '[workspace.package]\nversion = "%s"\n\n[workspace.dependencies]\nmango-demo = { path = "crates/demo", version = "%s" }\n' "$1" "$1" > "$readme_fixture/tree/Cargo.toml"
}
set_consumer_pin 0.3.0
if ! output=$("$readme_fixture/tree/scripts/check-versions.sh" 2>&1); then
  echo 'expected a consumer pin one release behind to pass, received:' >&2
  echo "$output" >&2
  exit 1
fi
set_workspace_version 0.3.2
if output=$("$readme_fixture/tree/scripts/check-versions.sh" 2>&1); then
  echo 'expected check-versions.sh to refuse a consumer pin two releases behind, received success' >&2
  exit 1
fi
case $output in
  *'received =0.3.0 (workspace 0.3.2)'*) ;;
  *)
    echo 'expected check-versions.sh to name pin 0.3.0 and workspace 0.3.2, received:' >&2
    echo "$output" >&2
    exit 1
    ;;
esac
set_workspace_version 0.3.1
set_consumer_pin 0.3.1
write_readme tree/crates/demo/README 0.3 0.1
if output=$("$readme_fixture/tree/scripts/check-versions.sh" 2>&1); then
  echo 'expected check-versions.sh to refuse a stale README snippet, received success' >&2
  exit 1
fi
case "$output" in
  *'crates/demo/README.md:6: expected mango-agent-acp = "0.3" for workspace version 0.3.1, received "0.1"'*) ;;
  *)
    echo 'expected check-versions.sh to name the stale README requirement, received:' >&2
    echo "$output" >&2
    exit 1
    ;;
esac
# A deleted README must fail, not vanish: a glob over READMEs would drop it from the arguments.
write_readme tree/crates/demo/README 0.3 0.3
mkdir -p "$readme_fixture/tree/crates/other"
printf '[package]\nname = "mango-other"\nversion.workspace = true\n' > "$readme_fixture/tree/crates/other/Cargo.toml"
# Rewritten whole with printf, not edited in place: `sed -i` needs a suffix argument on BSD sed and
# BSD sed does not expand `\n` in a replacement, so no single sed line works on both.
printf '[workspace.package]\nversion = "0.3.1"\n\n[workspace.dependencies]\nmango-other = { path = "crates/other", version = "0.3.1" }\nmango-demo = { path = "crates/demo", version = "0.3.1" }\n' > "$readme_fixture/tree/Cargo.toml"
if output=$("$readme_fixture/tree/scripts/check-versions.sh" 2>&1); then
  echo 'expected check-versions.sh to refuse a crate without a README, received success' >&2
  exit 1
fi
case "$output" in
  *'crates/other/README.md: expected a README file, received none'*) ;;
  *)
    echo 'expected check-versions.sh to name the missing README, received:' >&2
    echo "$output" >&2
    exit 1
    ;;
esac
rm -rf "$readme_fixture"
trap - EXIT
echo 'README install requirement checks passed'
