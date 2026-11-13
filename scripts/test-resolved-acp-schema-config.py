#!/usr/bin/env python3
"""Run offline Cargo against isolated config channels and a local replacement registry."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
CHECK = ROOT / 'scripts/check-resolved-acp-schema.py'
STATIC = ROOT / 'scripts/check-workspace-dependencies.py'


class CargoConfigFixture:
    def __init__(self, directory):
        self.root = Path(directory) / 'ancestor' / 'workspace'
        self.root.mkdir(parents=True)
        self.home = Path(directory) / 'cargo-home'
        self.home.mkdir()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith('CARGO_')}
        self.env['CARGO_HOME'] = str(self.home)
        self.member = self.root / 'crates/mango-agent-acp'
        self.member.mkdir(parents=True)
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers = ["crates/mango-agent-acp"]\nresolver = "2"\n')
        (self.member / 'Cargo.toml').write_text('[package]\nname = "mango-agent-acp"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nagent-client-protocol-schema = { version = "=1.11.0", default-features = false }\n')
        (self.member / 'src').mkdir()
        (self.member / 'src/lib.rs').write_text('')
        self.fork = self.root / 'fork'
        self.make_schema(self.fork)
        vendor = self.root / 'vendor'
        self.make_schema(vendor / 'agent-client-protocol-schema-1.11.0')
        (vendor / 'agent-client-protocol-schema-1.11.0/.cargo-checksum.json').write_text(json.dumps({'files': {}, 'package': '0' * 64}))
        # Equivalent local source avoids network access while retaining Cargo's crates.io identity.
        self.base = ['--config', 'source.crates-io.replace-with="local"', '--config', f'source.local.directory="{vendor}"']

    def make_schema(self, path):
        path.mkdir(parents=True)
        (path / 'Cargo.toml').write_text('[package]\nname = "agent-client-protocol-schema"\nversion = "1.11.0"\nedition = "2021"\n[workspace]\n')
        (path / 'src').mkdir()
        (path / 'src/lib.rs').write_text('')

    def run(self, args):
        return subprocess.run(args, cwd=self.root, env=self.env, text=True, capture_output=True)

    def patch(self):
        return f'[patch.crates-io]\nother = {{ package = "agent-client-protocol-schema", path = "{self.fork}" }}\n'

    def configure(self, channel):
        extra = []
        if channel == 'explicit':
            config = self.root / 'explicit.toml'
            config.write_text(self.patch())
            extra = ['--config', str(config)]
        else:
            path = {'workspace-toml': self.root / '.cargo/config.toml',
                    'workspace-extensionless': self.root / '.cargo/config',
                    'ancestor': self.root.parent / '.cargo/config.toml',
                    'cargo-home': self.home / 'config.toml'}[channel]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(self.patch())
        return extra


class ConfigRegressionTests(unittest.TestCase):
    def test_override_channels(self):
        for channel in ('workspace-toml', 'workspace-extensionless', 'ancestor', 'cargo-home', 'explicit'):
            with self.subTest(channel=channel), tempfile.TemporaryDirectory(prefix='resolved-acp-schema-') as temp:
                fixture = CargoConfigFixture(temp)
                extra = fixture.configure(channel)
                setup = fixture.run(['cargo', 'generate-lockfile', '--offline', *fixture.base, *extra])
                self.assertEqual(setup.returncode, 0, f'{channel}: lock setup failed: {setup.stderr}')
                static = fixture.run(['python3', str(STATIC), 'Cargo.toml', 'crates/mango-agent-acp/Cargo.toml'])
                self.assertEqual(static.returncode, 0, f'{channel}: static manifest policy: {static.stderr}')
                result = fixture.run(['python3', str(CHECK), *fixture.base, *extra])
                self.assertEqual(result.returncode, 1, f'{channel}: expected path refusal, received {result.returncode}: {result.stderr}')
                self.assertIn('expected crates.io registry identity, received path', result.stderr)

    def test_equivalent_registry_replacement(self):
        with tempfile.TemporaryDirectory(prefix='resolved-acp-schema-') as temp:
            fixture = CargoConfigFixture(temp)
            setup = fixture.run(['cargo', 'generate-lockfile', '--offline', *fixture.base])
            self.assertEqual(setup.returncode, 0, f'lock setup failed: {setup.stderr}')
            result = fixture.run(['python3', str(CHECK), *fixture.base])
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
