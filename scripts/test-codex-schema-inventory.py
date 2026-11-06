#!/usr/bin/env python3
"""Offline tests for variant ownership in the generated Codex inventory."""
import importlib.util
from pathlib import Path
import unittest
import sys

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location(
    "inventory", Path(__file__).with_name("codex-schema-inventory.py")
)
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class VariantInventoryTests(unittest.TestCase):
    def test_keeps_fields_and_required_names_with_their_variant(self):
        schema = {"oneOf": [
            {"properties": {"type": {"enum": ["subAgentActivity"]},
                            "id": {}, "agentThreadId": {}},
             "required": ["type", "id", "agentThreadId"]},
            {"properties": {"type": {"const": "collabAgentToolCall"},
                            "senderThreadId": {}, "receiverThreadIds": {}}},
        ]}
        self.assertEqual(inventory.variants(schema), {
            "subAgentActivity": {
                "properties": ["agentThreadId", "id", "type"],
                "required": ["agentThreadId", "id", "type"]},
            "collabAgentToolCall": {
                "properties": ["receiverThreadIds", "senderThreadId", "type"],
                "required": []},
        })

    def test_rejects_missing_or_ambiguous_discriminators(self):
        for tag in ({}, {"enum": ["a", "b"]}):
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "expected.*type"):
                inventory.variants({"oneOf": [{"properties": {"type": tag}}]})

    def test_rejects_duplicate_variants_instead_of_overwriting_evidence(self):
        branch = {"properties": {"type": {"const": "same"}}}
        with self.assertRaisesRegex(ValueError, "duplicate.*same"):
            inventory.variants({"oneOf": [branch, branch]})

    def test_requires_a_nonempty_variant_union(self):
        for schema in ({}, {"oneOf": []}):
            with self.subTest(schema=schema), self.assertRaisesRegex(ValueError, "oneOf"):
                inventory.variants(schema)


if __name__ == "__main__":
    unittest.main()
