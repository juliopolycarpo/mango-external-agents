---
name: Vendor drift
about: A vendor CLI moved (new version, changed flag, new event shape) and the harness must follow
title: "drift(<vendor>): "
labels: ["type: drift"]
---

## Vendor and versions

<!-- Pinned version in the harness crate vs the version observed. -->

## What changed

<!-- Diff from `mea capture`, the vendor changelog entry, or the doc page that moved. -->

## Compliance impact

<!-- Does the change touch a documented surface, an auth surface, or a policy statement? -->

## Layers touched

- [ ] harness crate
- [ ] fixtures (re-captured)
- [ ] `mea`
- [ ] docs
