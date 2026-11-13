#!/usr/bin/env python3
"""Check the ACP schema identity and features resolved by Cargo in this workspace.

Pass Cargo's --config options here and to the build when using explicit configuration.
"""
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

SCHEMA = 'agent-client-protocol-schema'
REGISTRY = 'registry+https://github.com/rust-lang/crates.io-index'
SAFE_VALUE = re.compile(r'[A-Za-z0-9_.+\-]+\Z')


class InvalidMetadata(Exception):
    """The resolved dependency graph does not satisfy the policy."""


def require_one(items, description):
    if len(items) != 1:
        raise InvalidMetadata(f'expected one {description}, received {len(items)}')
    return items[0]


def validate(metadata, expected_version):
    packages = metadata['packages']
    members = metadata['workspace_members']
    nodes = metadata['resolve']['nodes']
    if not isinstance(packages, list) or not isinstance(members, list) or not isinstance(nodes, list):
        raise InvalidMetadata(
            'expected metadata packages, workspace_members and resolve nodes as lists, received '
            f'{type(packages).__name__}, {type(members).__name__}, {type(nodes).__name__}'
        )
    acp = require_one([p for p in packages if p['name'] == 'mango-agent-acp' and p['id'] in members], 'workspace ACP package')
    acp_id = acp['id']
    require_one([p for p in packages if p['id'] == acp_id], 'ACP package ID')
    acp_node = require_one([n for n in nodes if n['id'] == acp_id], 'ACP resolve node')
    deps = acp_node['deps']
    if not isinstance(deps, list):
        raise InvalidMetadata(f'expected ACP dependency edges as a list, received {type(deps).__name__}')
    edge = require_one([d for d in deps if d['name'] == 'agent_client_protocol_schema'], 'ACP schema dependency edge')
    schema_id = edge['pkg']
    package = require_one([p for p in packages if p['id'] == schema_id], 'schema package ID')
    node = require_one([n for n in nodes if n['id'] == schema_id], 'schema resolve node')
    if package['name'] != SCHEMA:
        raise InvalidMetadata('expected ACP schema package, received another package')
    version = package['version']
    if not isinstance(version, str) or not SAFE_VALUE.fullmatch(version):
        raise InvalidMetadata(
            'expected a schema version of letters, digits, dots, plus signs and hyphens, '
            f'received a {type(version).__name__} outside that set'
        )
    if version != expected_version:
        raise InvalidMetadata(f'expected schema version {expected_version}, received {version}')
    source = package['source']
    if source != REGISTRY:
        if source is None:
            kind = 'path'
        elif isinstance(source, str) and source.startswith('git+'):
            kind = 'git'
        elif isinstance(source, str) and source.startswith(('registry+', 'sparse+')):
            kind = 'other registry'
        else:
            kind = 'unknown source'
        raise InvalidMetadata(f'expected crates.io registry identity, received {kind}')
    features = node['features']
    if not isinstance(features, list) or any(not isinstance(f, str) or not SAFE_VALUE.fullmatch(f) for f in features):
        raise InvalidMetadata('expected resolved schema features as a list of safe feature names, received another shape')
    if features:
        raise InvalidMetadata(f'expected no resolved schema features, received {features}')


def main(arguments):
    try:
        if len(arguments) % 2 or any(arguments[index] != '--config' for index in range(0, len(arguments), 2)):
            raise InvalidMetadata('expected zero or more --config VALUE pairs, received unsupported arguments')
        with Path('crates/mango-agent-acp/Cargo.toml').open('rb') as manifest:
            pin = tomllib.load(manifest)['dependencies'][SCHEMA]['version']
        if not isinstance(pin, str) or not pin.startswith('=') or not SAFE_VALUE.fullmatch(pin[1:]):
            raise InvalidMetadata('expected an exact schema pin such as =1.17.0 in crates/mango-agent-acp/Cargo.toml, received another requirement')
        result = subprocess.run(['cargo', 'metadata', '--format-version', '1', '--locked', '--all-features', *arguments], capture_output=True)
        if result.returncode:
            raise InvalidMetadata(f'expected successful cargo metadata, received exit status {result.returncode}')
        validate(json.loads(result.stdout), pin[1:])
    except InvalidMetadata as error:
        print(error, file=sys.stderr)
        return 1
    except (OSError, KeyError, TypeError, ValueError, UnicodeError):
        print('expected valid manifest and Cargo metadata graph, received malformed or unavailable input', file=sys.stderr)
        return 1
    print('ACP schema resolved to the pinned crates.io identity with no features')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
