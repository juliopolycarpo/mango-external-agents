# Contributing

Thanks for helping. `AGENTS.md` is the working guide; this file adds the human-facing bits.

- Open an issue before changing how a harness drives a vendor. Every such change cites the
  vendor document it follows and states its compliance impact in the pull request.
- Run `scripts/check.sh` before opening a pull request. It needs `cargo-nextest`, `cargo-deny`,
  `cargo-hack`, `dprint` and `git-cliff` on `PATH`.
- Commits follow Conventional Commits with a body and one concern per commit. The changelog is
  generated; do not edit it.
- Fixtures under `fixtures/` are captured with `mea capture`, never edited by hand.
- Documentation is English only.
