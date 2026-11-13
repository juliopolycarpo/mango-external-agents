#!/usr/bin/env python3
"""Offline tests for variant ownership in the generated Codex inventory."""
import copy
import importlib.util
import io
import json
import tempfile
from contextlib import ExitStack, redirect_stdout
from pathlib import Path
import unittest
import sys
from unittest import mock

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


class FakeSchemaGraphs:
    """Small app-server bundles with the same names as consumed Rust types."""

    def __init__(self):
        self.v1 = {"definitions": {}}
        self.v2 = {"definitions": {
            "ThreadStartParams": {
                "type": "object",
                "properties": {"cwd": {"type": "string"}},
                "required": ["cwd"],
            },
            "ThreadItem": {"oneOf": [
                {"type": "object", "properties": {
                    "type": {"const": "agentMessage"}, "id": {"type": "string"},
                    "text": {"type": "string"}}, "required": ["type", "id", "text"]},
                {"type": "object", "properties": {
                    "type": {"const": "reasoning"}, "id": {"type": "string"},
                    "summary": {"type": "array", "items": {"type": "string"}}},
                 "required": ["type", "id", "summary"]},
            ]},
        }}

    def projection(self):
        with tempfile.TemporaryDirectory(prefix="issue273-schema-") as directory:
            directory = Path(directory)
            v1 = directory / "v1.json"
            v2 = directory / "v2.json"
            v1.write_text(json.dumps(self.v1), encoding="utf-8")
            v2.write_text(json.dumps(self.v2), encoding="utf-8")
            with ExitStack() as stack:
                stack.enter_context(mock.patch.object(
                    inventory, "WANTED", ["ThreadStartParams", "ThreadItem"]))
                stack.enter_context(mock.patch.object(inventory, "WANTED_METHODS", {}))
                # The old entry point has only the broad name inventory. After the fix, exercise
                # the same entry point with a small allowlist instead of the production graph.
                if hasattr(inventory, "CONSUMED"):
                    stack.enter_context(mock.patch.object(inventory, "CONSUMED", {
                        "ThreadStartParams": ("v2", ["cwd"]),
                        "ThreadItem": ("v2", {
                            "agentMessage": ["type", "id", "text"],
                            "reasoning": ["type", "id", "summary"],
                        }),
                    }))
                stack.enter_context(mock.patch.object(sys, "argv", ["inventory", str(v2), str(v1), "test"]))
                output = io.StringIO()
                with redirect_stdout(output):
                    inventory.main()
            result = json.loads(output.getvalue())
            return result.get("consumed", result["definitions"])


class ConsumedProjectionRegressionTests(unittest.TestCase):
    def test_same_named_property_type_change_is_visible(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        fake.v2["definitions"]["ThreadStartParams"]["properties"]["cwd"]["type"] = "integer"
        self.assertNotEqual(before, fake.projection(), "expected cwd string to integer drift in projection")

    def test_same_named_property_requiredness_change_is_visible(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        fake.v2["definitions"]["ThreadStartParams"]["required"] = []
        self.assertNotEqual(before, fake.projection(), "expected cwd requiredness drift in projection")

    def test_same_named_property_nullability_change_is_visible(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        fake.v2["definitions"]["ThreadStartParams"]["properties"]["cwd"] = {
            "anyOf": [{"type": "string"}, {"type": "null"}]
        }
        self.assertNotEqual(before, fake.projection(), "expected nullable cwd drift in projection")

    def test_owned_variant_field_type_change_is_visible(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        fake.v2["definitions"]["ThreadItem"]["oneOf"][0]["properties"]["id"]["type"] = "integer"
        self.assertNotEqual(before, fake.projection(), "expected agentMessage.id type drift in projection")

    def test_description_and_unused_additions_do_not_change_projection(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        params = fake.v2["definitions"]["ThreadStartParams"]
        params["description"] = "changed description"
        params["properties"]["unread"] = {"type": "integer"}
        fake.v2["definitions"]["ThreadItem"]["oneOf"].append({
            "properties": {"type": {"const": "futureItem"}, "private": {"type": "string"}},
            "required": ["type", "private"],
        })
        self.assertEqual(before, fake.projection(),
                         "expected unread additions to leave consumed projection unchanged")


class FakeDivergentSchemaGraphs:
    """A v1 reference whose nested v2 target differs from the flat v2 target."""

    def __init__(self):
        self.v1 = {"definitions": {
            "Frame": {"type": "object", "properties": {
                "field": {"$ref": "#/definitions/v2/Thing"}}},
            "v2": {"Thing": {"type": "object", "properties": {
                "value": {"type": "integer"}}}},
        }}
        self.v2 = {"definitions": {"Thing": {"type": "object", "properties": {
            "value": {"type": "string"}}}}}

    def projection(self):
        with tempfile.TemporaryDirectory(prefix="issue273-divergent-") as directory:
            v1, v2 = Path(directory) / "v1.json", Path(directory) / "v2.json"
            v1.write_text(json.dumps(self.v1), encoding="utf-8")
            v2.write_text(json.dumps(self.v2), encoding="utf-8")
            with (mock.patch.object(inventory, "WANTED", ["Frame"]),
                  mock.patch.object(inventory, "WANTED_METHODS", {}),
                  mock.patch.object(inventory, "CONSUMED", {
                      "Frame": ("v1", ["field"]), "Thing": ("v2", ["value"])}),
                  mock.patch.object(sys, "argv", ["inventory", str(v2), str(v1), "test"])):
                output = io.StringIO()
                with redirect_stdout(output):
                    inventory.main()
            return json.loads(output.getvalue())["consumed"]


class DivergentReferenceRegressionTests(unittest.TestCase):
    def test_subsequent_shape_change_to_divergent_nested_target_is_visible(self):
        fake = FakeDivergentSchemaGraphs()
        before = fake.projection()
        fake.v1["definitions"]["v2"]["Thing"]["properties"]["value"]["type"] = "boolean"
        self.assertNotEqual(before, fake.projection(),
                            "expected nested v1 Thing.value integer to boolean drift")

    def test_same_named_direct_v1_target_retains_its_own_selected_shape(self):
        fake = FakeDivergentSchemaGraphs()
        fake.v1["definitions"]["Thing"] = fake.v1["definitions"]["v2"]["Thing"]
        fake.v1["definitions"]["Frame"]["properties"]["field"]["$ref"] = "#/definitions/Thing"
        before = fake.projection()
        fake.v1["definitions"]["Thing"]["properties"]["unread"] = {"type": "array"}
        self.assertEqual(before, fake.projection())
        fake.v1["definitions"]["Thing"]["properties"]["value"]["type"] = "boolean"
        self.assertNotEqual(before, fake.projection(), "expected direct v1 Thing.value type drift")


class FakeProductionNetworkSchemaGraphs:
    """Network approval schemas selected through the production allowlist."""

    def __init__(self):
        protocol = {"type": "string", "enum": ["http", "https", "socks5Tcp", "socks5Udp"]}
        self.v1 = {"definitions": {
            "CommandExecutionRequestApprovalParams": {
                "type": "object", "properties": {"networkApprovalContext": {"anyOf": [
                    {"$ref": "#/definitions/NetworkApprovalContext"}, {"type": "null"}]}}},
            "NetworkApprovalContext": {"type": "object", "properties": {
                "host": {"type": "string"},
                "protocol": {"$ref": "#/definitions/v2/NetworkApprovalProtocol"}},
                "required": ["host", "protocol"]},
            "v2": {"NetworkApprovalProtocol": copy.deepcopy(protocol)},
        }}
        self.v2 = {"definitions": {"NetworkApprovalProtocol": copy.deepcopy(protocol)}}

    def projection(self):
        projector = inventory.SchemaProjection(self.v1, self.v2)
        names = ("CommandExecutionRequestApprovalParams", "NetworkApprovalContext",
                 "NetworkApprovalProtocol")
        selected = {
            name: projector.project(inventory.CONSUMED[name][0], name,
                                    inventory.CONSUMED[name][1])
            for name in names if name in inventory.CONSUMED
        }
        selected["$referenced"] = projector.referenced
        return selected


class ProductionNetworkProjectionRegressionTests(unittest.TestCase):
    def test_network_context_nullability_is_visible(self):
        fake = FakeProductionNetworkSchemaGraphs()
        before = fake.projection()
        context = fake.v1["definitions"]["CommandExecutionRequestApprovalParams"]["properties"]
        context["networkApprovalContext"]["anyOf"].pop()
        self.assertNotEqual(before, fake.projection(),
                            "expected networkApprovalContext nullable-to-non-null drift")

    def test_network_host_and_protocol_field_types_are_visible(self):
        for name in ("host", "protocol"):
            with self.subTest(field=name):
                fake = FakeProductionNetworkSchemaGraphs()
                before = fake.projection()
                fake.v1["definitions"]["NetworkApprovalContext"]["properties"][name] = {
                    "type": "integer"}
                self.assertNotEqual(before, fake.projection(),
                                    f"expected NetworkApprovalContext.{name} type drift")

    def test_a_handled_network_protocol_change_is_visible(self):
        fake = FakeProductionNetworkSchemaGraphs()
        before = fake.projection()
        for definitions in (fake.v1["definitions"]["v2"], fake.v2["definitions"]):
            definitions["NetworkApprovalProtocol"]["enum"].remove("https")
        self.assertNotEqual(before, fake.projection(),
                            "expected the handled https protocol removal to change projection")

    def test_production_projection_records_network_owners_and_handled_protocols(self):
        selected = FakeProductionNetworkSchemaGraphs().projection()
        approval = selected["CommandExecutionRequestApprovalParams"]
        self.assertIn("networkApprovalContext", approval["schema"]["properties"])
        context = selected["NetworkApprovalContext"]
        self.assertEqual(context["source"], "v1#/definitions/NetworkApprovalContext")
        self.assertEqual(set(context["schema"]["properties"]), {"host", "protocol"})
        protocol = selected["NetworkApprovalProtocol"]
        self.assertEqual(protocol["source"], "v2#/definitions/NetworkApprovalProtocol")
        self.assertEqual(protocol["schema"]["enum"], ["http", "https", "socks5Tcp", "socks5Udp"])

    def test_unhandled_network_additions_do_not_change_the_projection(self):
        fake = FakeProductionNetworkSchemaGraphs()
        before = fake.projection()
        fake.v1["definitions"]["NetworkApprovalContext"]["properties"]["port"] = {
            "type": "integer"}
        for definitions in (fake.v1["definitions"]["v2"], fake.v2["definitions"]):
            definitions["NetworkApprovalProtocol"]["enum"].append("futureProtocol")
        self.assertEqual(before, fake.projection(),
                         "expected unhandled network fields and protocols to leave projection unchanged")


class SchemaProjectionTests(unittest.TestCase):
    def setUp(self):
        self.v1 = {
            "Value": {"type": "integer"},
            "v2": {
                "Thing": {"type": "object", "properties": {
                    "value": {"$ref": "#/definitions/v2/Value"}}},
                "Value": {"type": "string"},
            },
        }
        self.v2 = {
            "Value": {"type": "string"},
            "Thing": {"type": "object", "properties": {
                "value": {"$ref": "#/definitions/Value"}}},
        }
        self.projector = inventory.SchemaProjection({"definitions": self.v1}, {"definitions": self.v2})

    def test_reference_resolves_from_its_own_root(self):
        self.assertEqual(self.projector.reference("v1", "#/definitions/Value"), "v1#/definitions/Value")
        self.assertEqual(self.projector.reference("v2", "#/definitions/Value"), "v2#/definitions/Value")
        self.assertEqual(self.projector.resolve("v1", "#/definitions/Value"), {"type": "integer"})
        self.assertEqual(self.projector.resolve("v2", "#/definitions/Value"), {"type": "string"})

    def test_nested_v2_reference_is_canonical_only_after_resolving_equivalence(self):
        with mock.patch.object(inventory, "CONSUMED", {"Thing": ("v2", ["value"])}):
            pointer = "#/definitions/v2/Thing"
            self.assertEqual(self.projector.reference("v1", pointer), "v2#/definitions/Thing")
            self.v1["v2"]["Thing"]["properties"]["unread"] = {"type": "number"}
            self.assertEqual(self.projector.reference("v1", pointer), "v2#/definitions/Thing")
            self.v1["v2"]["Value"]["type"] = "integer"
            self.assertEqual(self.projector.reference("v1", pointer), "v1" + pointer)

    def test_direct_collision_is_not_confused_with_a_nested_mirror(self):
        self.v1["Thing"] = {"type": "number"}
        with mock.patch.object(inventory, "CONSUMED", {"Thing": ("v2", ["value"])}):
            self.assertEqual(self.projector.reference("v1", "#/definitions/Thing"), "v1#/definitions/Thing")
            self.assertEqual(self.projector.reference("v1", "#/definitions/v2/Thing"),
                             "v2#/definitions/Thing")
            self.assertEqual(self.projector.project("v1", "Thing", [])['schema'], {"type": "number"})

    def test_missing_flat_mirror_and_recursive_mirror_keep_correct_ownership(self):
        self.v1["v2"]["Thing"]["properties"]["value"] = {"$ref": "#/definitions/v2/Thing"}
        self.v2["Thing"]["properties"]["value"] = {"$ref": "#/definitions/Thing"}
        with mock.patch.object(inventory, "CONSUMED", {"Thing": ("v2", ["value"])}):
            self.assertEqual(self.projector.reference("v1", "#/definitions/v2/Thing"),
                             "v2#/definitions/Thing")
            del self.v2["Thing"]
            self.assertEqual(self.projector.reference("v1", "#/definitions/v2/Thing"),
                             "v1#/definitions/v2/Thing")

    def test_pointer_escapes_and_invalid_references(self):
        self.v1["a/b~c"] = {"anyOf": [{"type": "boolean"}]}
        self.assertEqual(self.projector.resolve("v1", "#/definitions/a~1b~0c/anyOf/0"), {"type": "boolean"})
        self.assertEqual(self.projector.project("v1", "a/b~c", {"$shape": True})["source"],
                         "v1#/definitions/a~1b~0c")
        for pointer in ("http://example.invalid/schema", "#/definitions/missing"):
            with self.subTest(pointer=pointer), self.assertRaisesRegex(ValueError, "expected.*received"):
                self.projector.reference("v1", pointer)

    def test_defs_bundle_references_use_original_root_path(self):
        v1 = {"definitions": {}, "$defs": {"v2": {"Value": {"type": "string"}}}}
        v2 = {"$defs": {"Value": {"type": "string"}}}
        projector = inventory.SchemaProjection(v1, v2)
        self.assertIs(projector.resolve("v1", "#"), v1)
        self.assertEqual(projector.reference("v1", "#/$defs/v2/Value"), "v2#/$defs/Value")
        self.assertEqual(projector.project("v2", "Value", {"$shape": True})["source"],
                         "v2#/$defs/Value")

    def test_schema_shape_preserves_selected_structure_and_ignores_unselected_properties(self):
        schema = {"type": "object", "properties": {
            "selected": {"type": ["string", "null"]},
            "unused": {"type": "integer"}}, "required": ["selected", "unused"]}
        self.assertEqual(self.projector.shape(schema, "v1", ["selected"]), {
            "type": "object", "properties": {"selected": {
                "required": True, "schema": {"type": ["null", "string"]}}}})
        self.assertEqual(self.projector.shape(True, "v1"), True)
        with self.assertRaisesRegex(ValueError, "expected a schema.*received"):
            self.projector.shape("bad schema", "v1")

    def test_nested_object_alternatives_keep_only_the_selected_url(self):
        image = {"type": "object", "properties": {"type": {"const": "image"}},
                 "required": ["type"], "anyOf": [
                     {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]},
                     {"type": "object", "properties": {"fileId": {"type": "string"}}, "required": ["fileId"]},
                 ]}
        self.v2["Image"] = {"oneOf": [image]}
        projected = self.projector.project("v2", "Image", {"image": ["type", "url"]})
        image_shape = projected["schema"]["variants"]["image"]
        self.assertTrue(image_shape["anyOf"][0]["properties"]["url"]["required"])
        self.assertNotIn("fileId", json.dumps(image_shape))
        before = copy.deepcopy(projected)
        image["anyOf"][1]["properties"]["unread"] = {"type": "number"}
        self.assertEqual(before, self.projector.project("v2", "Image", {"image": ["type", "url"]}))
        image["anyOf"][0]["properties"]["url"]["type"] = "number"
        self.assertNotEqual(before, self.projector.project("v2", "Image", {"image": ["type", "url"]}))

    def test_external_choices_and_known_enum_values_have_owned_shapes(self):
        self.v1["Decision"] = {"oneOf": [
            {"type": "string", "enum": ["accept", "decline", "future"]},
            {"type": "object", "properties": {"amend": {"type": "object", "properties": {
                "rule": {"type": "array", "items": {"type": "string"}}}, "required": ["rule"]}},
             "required": ["amend"], "additionalProperties": False},
        ]}
        selected = {"$enum": ["accept", "decline"], "$objects": {"amend": ["rule"]}}
        before = self.projector.project("v1", "Decision", selected)
        result = before["schema"]
        self.assertEqual(result["oneOf"][0]["enum"], ["accept", "decline"])
        self.assertTrue(result["objects"]["amend"]["properties"]["amend"]["required"])
        self.v1["Decision"]["oneOf"][0]["enum"].append("anotherFuture")
        self.assertEqual(before, self.projector.project("v1", "Decision", selected))
        rule = self.v1["Decision"]["oneOf"][1]["properties"]["amend"]["properties"]["rule"]
        rule["items"]["type"] = "integer"
        self.assertNotEqual(before, self.projector.project("v1", "Decision", selected))

    def test_scalar_alias_records_type_without_enumerating_unused_values(self):
        self.v2["StringAlias"] = {"type": "string", "enum": ["one"]}
        selected = {"$shape": True}
        before = self.projector.project("v2", "StringAlias", selected)
        self.v2["StringAlias"]["enum"].append("future")
        self.assertEqual(before, self.projector.project("v2", "StringAlias", selected))
        self.v2["StringAlias"]["enum"].append(None)
        self.assertNotEqual(before, self.projector.project("v2", "StringAlias", selected))
        self.v2["StringAlias"]["enum"].remove(None)
        self.v2["StringAlias"]["type"] = "integer"
        self.assertNotEqual(before, self.projector.project("v2", "StringAlias", selected))

    def test_type_constraints_are_visible_for_selected_fields(self):
        self.v2["Frame"] = {"type": "object", "properties": {
            "count": {"type": "integer", "format": "uint32", "minimum": 0}},
            "required": ["count"]}
        before = self.projector.project("v2", "Frame", ["count"])
        self.assertEqual(before["schema"]["properties"]["count"]["schema"]["format"], "uint32")
        self.v2["Frame"]["properties"]["count"]["format"] = "uint64"
        self.assertNotEqual(before, self.projector.project("v2", "Frame", ["count"]))

    def test_duplicate_selected_variant_is_rejected(self):
        branch = {"properties": {"type": {"const": "known"}}}
        self.v2["Choice"] = {"oneOf": [branch, branch]}
        with self.assertRaisesRegex(ValueError, "expected unique.*duplicate 'known'"):
            self.projector.project("v2", "Choice", {"known": ["type"]})

    def test_variant_field_ownership_survives_a_field_swap(self):
        fake = FakeSchemaGraphs()
        before = fake.projection()
        a, b = fake.v2["definitions"]["ThreadItem"]["oneOf"]
        a["properties"]["text"], b["properties"]["summary"] = (
            b["properties"]["summary"], a["properties"]["text"])
        self.assertNotEqual(before, fake.projection(), "expected per-variant field schema drift")

    def test_selected_field_the_bundle_lacks_is_recorded_as_undeclared(self):
        self.v2["Typo"] = {"type": "object", "properties": {"cwd": {"type": "string"}}}
        schema = self.projector.project("v2", "Typo", ["cwd", "cwdd"])["schema"]
        self.assertEqual(schema["undeclared"], ["cwdd"])
        self.assertEqual(list(schema["properties"]), ["cwd"])
        clean = self.projector.project("v2", "Typo", ["cwd"])["schema"]
        self.assertNotIn("undeclared", clean)

    def test_selected_enum_value_the_bundle_lacks_is_recorded_as_undeclared(self):
        self.v2["Status"] = {"type": "string", "enum": ["inProgress", "completed"]}
        schema = self.projector.project(
            "v2", "Status", {"$enum": ["inProgress", "completed", "declined"]})["schema"]
        self.assertEqual(schema["undeclared"], ["declined"])
        self.assertEqual(schema["enum"], ["completed", "inProgress"])

    def test_variant_field_the_variant_lacks_is_recorded_on_that_variant_only(self):
        self.v2["Item"] = {"oneOf": [
            {"type": "object", "properties": {"type": {"const": "a"}, "status": {"type": "string"}}},
            {"type": "object", "properties": {"type": {"const": "b"}}},
        ]}
        variants = self.projector.project(
            "v2", "Item", {"a": ["type", "status"], "b": ["type", "status"]})["schema"]["variants"]
        self.assertNotIn("undeclared", variants["a"])
        self.assertEqual(variants["b"]["undeclared"], ["status"])

    def test_external_object_field_the_choice_lacks_is_recorded_as_undeclared(self):
        self.v2["Decision"] = {"oneOf": [
            {"type": "object", "properties": {"amend": {"type": "object"}}, "required": ["amend"]},
        ]}
        objects = self.projector.project(
            "v2", "Decision", {"$enum": [], "$objects": {"amend": ["rule"]}})["schema"]["objects"]
        self.assertEqual(objects["amend"]["undeclared"], ["rule"])


class ProductionUndeclaredSelectionTests(unittest.TestCase):
    """The committed projection names every allowlisted value the pinned build lacks."""

    SCHEMA = Path(__file__).resolve().parent.parent / "crates/mango-agent-codex/vendor/schema.json"
    # Values the allowlist selects ahead of the pinned build. A typo in the allowlist would add a
    # name here; a vendor release that declares one removes it and shows up as drift.
    EXPECTED = {
        ("CollabAgentToolCallStatus",): ["declined"],
        ("DynamicToolCallStatus",): ["declined"],
        ("McpToolCallStatus",): ["declined"],
        ("ThreadItem", "userMessage"): ["status"],
        ("ThreadItem", "hookPrompt"): ["status"],
        ("ThreadItem", "functionCallOutput"): ["status"],
        ("ThreadItem", "imageView"): ["status"],
        ("ThreadItem", "sleep"): ["status"],
    }

    @staticmethod
    def undeclared(node, path=()):
        if isinstance(node, dict):
            for key, child in node.items():
                if key == "undeclared":
                    yield path, child
                elif key in ("variants", "objects", "properties", "schema"):
                    yield from ProductionUndeclaredSelectionTests.undeclared(child, path)
                else:
                    yield from ProductionUndeclaredSelectionTests.undeclared(child, path + (key,))

    def test_committed_projection_lists_exactly_the_known_undeclared_selections(self):
        consumed = json.loads(self.SCHEMA.read_text(encoding="utf-8"))["consumed"]
        found = {}
        for name, entry in consumed.items():
            if name == "$referenced":
                continue
            for path, names in self.undeclared(entry["schema"], (name,)):
                found[path] = names
        self.assertEqual(
            found, self.EXPECTED,
            f"(undeclared selections): expected {self.EXPECTED} | received {found}")


if __name__ == "__main__":
    unittest.main()
