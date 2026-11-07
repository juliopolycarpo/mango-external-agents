#!/usr/bin/env python3
"""Profile the exact baseline/candidate encoder bodies outside the SDK workspace."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--base', default='b0e68ecb74e6604b2b68a2b387a23081055b74b3')
parser.add_argument('--candidate', default='HEAD')
options = parser.parse_args()
source = 'crates/mango-agent-codex/src/session.rs'
artifacts = Path(__file__).resolve().parent
work = Path(tempfile.mkdtemp(prefix='codex-data-url-profile.'))
print(f'Profiling workspace: {work}', flush=True)
(work / 'src').mkdir()
shutil.copyfile(artifacts / 'probe.rs', work / 'src/main.rs')
shutil.copyfile(artifacts / 'probe-Cargo.toml', work / 'Cargo.toml')
shutil.copyfile(artifacts / 'probe-Cargo.lock', work / 'Cargo.lock')
identities = []
sources = {}
for label, revision in [('base', options.base), ('variant', options.candidate)]:
    identity = subprocess.check_output(['git', 'rev-parse', revision], text=True).strip()
    text = subprocess.check_output(['git', 'show', f'{identity}:{source}'], text=True)
    start = text.index('fn data_url(')
    end = text.index('\n#[async_trait::async_trait]', start)
    body = text[start:end].replace('fn data_url(', 'pub fn data_url(', 1)
    (work / f'src/{label}.rs').write_text(body)
    identities.append(f'{label} {identity} {hashlib.sha256(body.encode()).hexdigest()}')
    sources[label] = identity
(work / 'identities.txt').write_text('\n'.join(identities) + '\n')
environment = os.environ.copy()
environment['CARGO_TARGET_DIR'] = str(work / 'target')
environment.setdefault('CARGO_BUILD_JOBS', '2')
command = ['cargo', 'build', '--release', '--locked', '--manifest-path', str(work / 'Cargo.toml')]
started = datetime.now(timezone.utc).isoformat()
with (work / 'build.log').open('w') as log:
    result = subprocess.run(command, stdout=log, stderr=log, env=environment)
with (work / 'attempts.jsonl').open('a') as audit:
    audit.write(json.dumps({'stage': 'build', 'command': command, 'cwd': os.getcwd(), 'started_at': started, 'completed_at': datetime.now(timezone.utc).isoformat(), 'exit': result.returncode, 'log': 'build.log'}) + '\n')
result.check_returncode()
binary = work / 'target/release/codex-data-url-allocation-probe'
binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
(work / 'artifact.json').write_text(json.dumps({'binary_sha256': binary_hash, 'probe_sha256': hashlib.sha256((work / 'src/main.rs').read_bytes()).hexdigest(), 'lock_sha256': hashlib.sha256((work / 'Cargo.lock').read_bytes()).hexdigest(), 'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'sources': sources}, indent=2) + '\n')
command = [str(binary), 'equal']
started = datetime.now(timezone.utc).isoformat()
with (work / 'equality.log').open('w') as log:
    result = subprocess.run(command, stdout=log, stderr=log)
with (work / 'attempts.jsonl').open('a') as audit:
    audit.write(json.dumps({'stage': 'equality', 'command': command, 'cwd': os.getcwd(), 'started_at': started, 'completed_at': datetime.now(timezone.utc).isoformat(), 'exit': result.returncode, 'binary_sha256': binary_hash, 'log': 'equality.log'}) + '\n')
result.check_returncode()
with (work / 'raw.csv').open('w') as out, (work / 'attempts.log').open('w') as log:
    out.write('pair,order,side,input_bytes,output_bytes,total_blocks,total_bytes,max_blocks,max_bytes\n')
    for size in [0, 1, 2, 3, 1024, 65536, 2097152]:
        for pair, order in enumerate([('base', 'variant'), ('variant', 'base')], 1):
            for position, side in enumerate(order, 1):
                command = [str(binary), side, str(size)]
                started = datetime.now(timezone.utc).isoformat()
                result = subprocess.run(command, capture_output=True, text=True)
                log.write(f'{size}/{pair}/{position} exit={result.returncode}\n{result.stdout}{result.stderr}')
                with (work / 'attempts.jsonl').open('a') as audit:
                    audit.write(json.dumps({'stage': 'profile', 'command': command, 'cwd': os.getcwd(), 'started_at': started, 'completed_at': datetime.now(timezone.utc).isoformat(), 'size': size, 'pair': pair, 'order': position, 'side': side, 'source': sources[side], 'binary_sha256': binary_hash, 'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}) + '\n')
                result.check_returncode()
                out.write(f'{pair},{position},' + result.stdout)
print(f'Allocation profiles and all logs retained in {work}')
