#!/usr/bin/env python3
"""Exercise package fixture synchronization without changing captures."""
import importlib.util
from pathlib import Path
import tempfile
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("package_fixtures", Path(__file__).with_name("package-fixtures.py"))
package_fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package_fixtures)


class PackageFixtureTests(unittest.TestCase):
    def test_missing_stale_and_extra_copies_are_detected_and_repaired(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "fixtures" / "claude" / "contract"
            source.mkdir(parents=True)
            (source / "manifest.json").write_bytes(b"manifest")
            (source / "capture.json").write_bytes(b"capture")
            destination = root / "crates" / "mango-agent-claude" / "fixtures" / "claude" / "contract"
            with patch.object(package_fixtures, "SELECTIONS", {"claude": ["contract"]}):
                package_fixtures.synchronize(root, False)
                self.assertEqual(package_fixtures.synchronize(root, True), [])
                (destination / "manifest.json").unlink()
                (destination / "capture.json").write_bytes(b"stale")
                (destination / "extra.json").write_bytes(b"extra")
                differences = "\n".join(package_fixtures.synchronize(root, True))
                self.assertIn("manifest.json: expected fixture copy, received missing file", differences)
                self.assertIn("capture.json: expected bytes from", differences)
                self.assertIn("extra.json: expected no extra fixture, received file", differences)
                package_fixtures.synchronize(root, False)
                self.assertEqual(package_fixtures.synchronize(root, True), [])

    def test_missing_canonical_selection_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "missing.json: expected fixture selection, received no files"):
                package_fixtures.selected_files(Path(directory), ["missing.json"])

    def test_glob_picks_up_new_transcripts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "first.jsonl").write_bytes(b"first")
            (root / "second.jsonl").write_bytes(b"second")
            (root / "nested").mkdir()
            (root / "nested" / "third.jsonl").write_bytes(b"third")
            self.assertEqual(package_fixtures.selected_files(root, ["**/*.jsonl"]), {Path("first.jsonl"), Path("second.jsonl"), Path("nested/third.jsonl")})

    def test_claude_selection_packages_a_new_transcript_without_naming_it(self):
        """A recording the recorder adds under transcripts/ must reach the published crate."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "fixtures" / "claude"
            # Every other Claude selection has to exist, or selected_files rejects the tree.
            for relative in ("help/2.1.227.txt", "help/2.1.260.txt", "help/2.1.270.txt",
                             "contract/manifest.json", "historical/contract/manifest.json"):
                (source / relative).parent.mkdir(parents=True, exist_ok=True)
                (source / relative).write_bytes(b"fixed")
            transcripts = source / "transcripts"
            transcripts.mkdir()
            for name in ("read-turn", "denied-write-turn", "structured-output"):
                (transcripts / f"{name}.jsonl").write_bytes(name.encode())
            (transcripts / "notes.txt").write_bytes(b"not a transcript")
            with patch.object(package_fixtures, "SELECTIONS", {"claude": package_fixtures.SELECTIONS["claude"]}):
                package_fixtures.synchronize(root, False)
                self.assertEqual(package_fixtures.synchronize(root, True), [])
                packaged = root / "crates" / "mango-agent-claude" / "fixtures" / "claude" / "transcripts"
                self.assertEqual(sorted(path.name for path in packaged.iterdir()),
                                 ["denied-write-turn.jsonl", "read-turn.jsonl", "structured-output.jsonl"])


if __name__ == "__main__":
    unittest.main()
