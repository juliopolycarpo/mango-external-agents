#!/usr/bin/env python3
"""Exercise resolved schema policy through a named fake Cargo executable."""
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
CHECK = ROOT / 'scripts/check-resolved-acp-schema.py'
SOURCE = 'registry+https://github.com/rust-lang/crates.io-index'
# The manifest's exact pin, so a schema bump does not need an edit here.
with (ROOT / 'crates/mango-agent-acp/Cargo.toml').open('rb') as manifest:
    PINNED_VERSION = tomllib.load(manifest)['dependencies']['agent-client-protocol-schema']['version'].removeprefix('=')


class FakeCargo:
    def __init__(self, directory):
        self.directory = Path(directory)
        self.output = self.directory / 'metadata.json'
        self.executable = self.directory / 'cargo'
        self.executable.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$FAKE_CARGO_ARGS"\ncat "$FAKE_CARGO_OUTPUT"\nprintf "sensitive stderr token\\n" >&2\nexit "${FAKE_CARGO_STATUS:-0}"\n')
        self.executable.chmod(0o755)

    def run(self, metadata, status=0, extra=()):
        self.output.write_text(metadata if isinstance(metadata, str) else json.dumps(metadata))
        env = os.environ | {'PATH': str(self.directory) + os.pathsep + os.environ['PATH'],
                            'FAKE_CARGO_OUTPUT': str(self.output),
                            'FAKE_CARGO_ARGS': str(self.directory / 'args'),
                            'FAKE_CARGO_STATUS': str(status)}
        return subprocess.run(['python3', str(CHECK), *extra], cwd=ROOT, env=env,
                              capture_output=True, text=True)


def valid_metadata():
    return {'packages': [{'id': 'acp', 'name': 'mango-agent-acp'},
                         {'id': 'schema', 'name': 'agent-client-protocol-schema',
                          'version': PINNED_VERSION, 'source': SOURCE}],
            'workspace_members': ['acp'],
            'resolve': {'nodes': [{'id': 'acp', 'deps': [{'name': 'agent_client_protocol_schema', 'pkg': 'schema'}]},
                                  {'id': 'schema', 'features': []}]}}


class ResolvedSchemaTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cargo = FakeCargo(self.temp.name)
        self.metadata = valid_metadata()

    def assert_refused(self, text):
        result = self.cargo.run(self.metadata)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(text, result.stderr)
        self.assertIn('expected', result.stderr)
        self.assertIn('received', result.stderr)

    def test_accepts_registry_and_passes_context(self):
        result = self.cargo.run(self.metadata, extra=('--config', 'net.offline=true'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((Path(self.temp.name) / 'args').read_text().splitlines(),
                         ['metadata', '--format-version', '1', '--locked', '--all-features', '--config', 'net.offline=true'])

    def test_sources_and_version(self):
        for source, kind in [(None, 'path'), ('git+https://secret:token@example.test/a', 'git'),
                             ('registry+https://secret:token@example.test/a', 'other registry'),
                             ('sparse+https://secret:token@example.test/a', 'other registry')]:
            with self.subTest(source=source):
                self.metadata['packages'][1]['source'] = source
                result = self.cargo.run(self.metadata)
                self.assertEqual(result.returncode, 1)
                self.assertIn(kind, result.stderr)
                self.assertNotIn('secret', result.stderr)
                self.assertNotIn('token', result.stderr)
        self.metadata['packages'][1]['source'] = SOURCE
        self.metadata['packages'][1]['version'] = '9.0.0'
        self.assert_refused('9.0.0')

    def test_features(self):
        self.metadata['resolve']['nodes'][1]['features'] = ['unstable']
        self.assert_refused('unstable')

    def test_missing_and_duplicate_graph_entries(self):
        cases = []
        for key, item in [('packages', self.metadata['packages'][0]), ('packages', self.metadata['packages'][1])]:
            missing = copy.deepcopy(self.metadata); missing[key].remove(item); cases.append(missing)
            duplicate = copy.deepcopy(self.metadata); duplicate[key].append(copy.deepcopy(item)); cases.append(duplicate)
        for index in (0, 1):
            missing = copy.deepcopy(self.metadata); missing['resolve']['nodes'].pop(index); cases.append(missing)
            duplicate = copy.deepcopy(self.metadata); duplicate['resolve']['nodes'].append(copy.deepcopy(duplicate['resolve']['nodes'][index])); cases.append(duplicate)
        missing = copy.deepcopy(self.metadata); missing['resolve']['nodes'][0]['deps'] = []; cases.append(missing)
        duplicate = copy.deepcopy(self.metadata); duplicate['resolve']['nodes'][0]['deps'] *= 2; cases.append(duplicate)
        for case in cases:
            with self.subTest(case=case):
                self.metadata = case
                self.assert_refused('received')

    def test_malformed_shapes_name_the_expected_shape(self):
        # Each shape error states what was expected and the received type, never the raw value.
        cases = [
            ({'packages': 'x', 'workspace_members': None, 'resolve': {'nodes': {}}},
             'expected metadata packages, workspace_members and resolve nodes as lists, received str, NoneType, dict'),
        ]
        deps = copy.deepcopy(self.metadata); deps['resolve']['nodes'][0]['deps'] = 'secret'
        cases.append((deps, 'expected ACP dependency edges as a list, received str'))
        version = copy.deepcopy(self.metadata); version['packages'][1]['version'] = 'secret token'
        cases.append((version, 'expected a schema version of letters, digits, dots, plus signs and hyphens, received a str outside that set'))
        features = copy.deepcopy(self.metadata); features['resolve']['nodes'][1]['features'] = ['secret token']
        cases.append((features, 'expected resolved schema features as a list of safe feature names, received another shape'))
        for metadata, expected in cases:
            with self.subTest(expected=expected):
                result = self.cargo.run(metadata)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(expected, result.stderr)
                self.assertNotIn('secret', result.stderr)

    def test_malformed_and_cargo_failure(self):
        for data in ('{', '{}', json.dumps({'packages': None}), json.dumps(valid_metadata()).replace('"features": []', '"features": null')):
            result = self.cargo.run(data)
            self.assertEqual(result.returncode, 1)
            self.assertIn('expected', result.stderr)
            self.assertIn('received', result.stderr)
        result = self.cargo.run('sensitive raw cargo output', status=7)
        self.assertEqual(result.returncode, 1)
        self.assertIn('received exit status 7', result.stderr)
        self.assertNotIn('sensitive', result.stderr)
        result = self.cargo.run(self.metadata, extra=('--manifest-path', 'other'))
        self.assertEqual(result.returncode, 1)
        self.assertIn('expected zero or more --config VALUE pairs, received unsupported arguments', result.stderr)


if __name__ == '__main__':
    unittest.main()
