## Summary

<!-- What changes for a host that embeds the library, or for a harness's behaviour. -->

## Changes

<!-- Concrete changes, grouped by crate: core, claude, codex, acp, mea, fixtures, docs. -->

## Compliance impact

- [ ] No vendor surface changes, or the change stays on a documented surface (cite it below)
- [ ] No login handling was added; no credential is read, stored or forwarded
- [ ] `docs/compliance.md` and `docs/harness-<vendor>.md` updated if the posture moved

<!-- Vendor document followed: -->

## Test Plan

- [ ] `scripts/check.sh` passes
- [ ] Fixtures re-captured with `mea capture` where a vendor dialect changed
- [ ] Additive on the public API, or the minor was bumped (0.x semver)

## Notes

<!-- Breaking changes, deliberate behaviour changes from the mangostudio port, open questions. -->
