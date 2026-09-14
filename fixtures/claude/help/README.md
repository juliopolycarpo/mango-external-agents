# Historical `claude --help` captures

Three files, and the difference between them is the point. A single regenerated fixture would
delete the older half of every pair, and the older half is what proves that a build predating a
feature keeps working with that feature simply off.

| File           | What it is                                     | Captured   |
| -------------- | ---------------------------------------------- | ---------- |
| `2.1.270.txt`  | `claude --help` in full, verbatim               | 2026-09-13 |
| `2.1.260.txt`  | an excerpt, kept in the shape commander prints  | 2026-09-04 |
| `2.1.227.txt`  | an excerpt from the build before the pair below | 2026-08-11 |

The two excerpts are trimmed to the options the harness reads plus enough neighbours to exercise
the parser's real problems. All three files are deliberately **not** re-captured:

- A description that *mentions* a flag it does not declare (`--forward-subagent-text` names
  `--output-format=stream-json`), which is what a parser scanning the whole text would report as
  declared.
- An option whose flags wrap onto their own line (`--allowedTools, --allowed-tools`).
- A choice list long enough that commander wraps it mid-list (`--permission-mode`).
- Two traps in `--model`'s prose on 2.1.260 that must not be tidied: the alias list and the
  full-name example live in two different `(e.g. …)` groups, and the apostrophe in "model's" opens
  a quote a naive scan closes against the next one.
- `--effort` and `--permission-prompts` do not exist at all on 2.1.227, which is what proves an
  older build keeps working with those features off; `--model` on 2.1.227 is the bare "Model for
  the current session.", which is what proves an absent catalog stays absent instead of becoming an
  empty one.

`2.1.270.txt` is the full 2026-09-13 capture, kept to test the parser against the observed layout.
It is archival, not the drift artifact. `mea capture --harness claude` regenerates the current
public contract in `fixtures/claude/contract/`; see the [fixture rules](../../README.md).
