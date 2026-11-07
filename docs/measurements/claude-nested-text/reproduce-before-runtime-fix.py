import datetime
import hashlib
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

OUT = Path('/workspace/work/pr-wave')
ROOT = OUT / 'perf-261-probe'
EVIDENCE = OUT / 'perf-261-results'
AUDIT = EVIDENCE / 'attempts.jsonl'
SCENARIOS = ['ordinary-ascii', 'ordinary-unicode', 'oversized-ascii',
             'oversized-unicode', 'grow-then-oversized', 'full-then-ignored']
SIDES = {side: ROOT / side for side in ['base', 'variant']}
SDK = {side: OUT / ('perf-261-' + side) for side in SIDES}

def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def command(label, args, cwd, timeout=600):
    stdout = EVIDENCE / (label + '.stdout')
    stderr = EVIDENCE / (label + '.stderr')
    record = {'attempt': label, 'start': stamp(), 'command': args, 'cwd': str(cwd),
              'stdout': stdout.name, 'stderr': stderr.name}
    with stdout.open('w') as out, stderr.open('w') as err:
        try:
            result = subprocess.run(args, cwd=cwd, stdout=out, stderr=err, timeout=timeout)
            record['exit'] = result.returncode
        except subprocess.TimeoutExpired:
            record['exit'] = 'timeout'
    record['end'] = stamp()
    record['stdout_sha256'] = sha(stdout)
    record['stderr_sha256'] = sha(stderr)
    with AUDIT.open('a') as audit:
        audit.write(json.dumps(record) + '\n')
    print(label, record['exit'], flush=True)
    assert record['exit'] == 0, record
    return stdout.read_text()

def prepare():
    assert not AUDIT.exists(), 'preserve existing attempts; use a newly labeled experiment'
    plan = json.loads((OUT / 'perf-261-plan.json').read_text())
    assert sha(OUT / 'perf-261-probe.rs') == plan['probe_sha256']
    for side, path in SIDES.items():
        path.mkdir(parents=True, exist_ok=True)
        (path / 'src').mkdir(exist_ok=True)
        shutil.copy2(OUT / 'perf-261-probe.rs', path / 'src/main.rs')
        manifest = f'''[package]
name = "nested-text-allocation-probe"
version = "0.0.0"
edition = "2024"
[workspace]
[dependencies]
mango-agent-claude = {{ path = "{SDK[side]}/crates/mango-agent-claude", default-features = false }}
dhat = "=0.3.3"
serde_json = "1"
sha2 = "=0.10.9"
[profile.release]
debug = 1
'''
        (path / 'Cargo.toml').write_text(manifest)
        if side == 'base':
            shutil.copy2(Path(__file__).with_name('probe-Cargo.lock'), path / 'Cargo.lock')
            command('setup-lock-base', ['cargo', 'metadata', '--locked', '--format-version=1',
                    '--manifest-path', str(path / 'Cargo.toml')], path)
        else:
            shutil.copy2(SIDES['base'] / 'Cargo.lock', path / 'Cargo.lock')
        command('setup-build-' + side, ['python3', '/workspace/.cloud-tools/reap.py', 'cargo',
                'build', '--release', '--locked', '--manifest-path', str(path / 'Cargo.toml')], path)
        command('setup-metadata-' + side, ['cargo', 'metadata', '--locked', '--format-version=1',
                '--manifest-path', str(path / 'Cargo.toml')], path)
        command('setup-correctness-' + side, [str(binary(side)), 'correctness'], path, 60)
    assert sha(SIDES['base'] / 'Cargo.lock') == sha(SIDES['variant'] / 'Cargo.lock')
    base = (EVIDENCE / 'setup-correctness-base.stdout').read_bytes()
    variant = (EVIDENCE / 'setup-correctness-variant.stdout').read_bytes()
    assert base == variant, 'serialized event digests/event counts differ'
    assert len(base.decode().splitlines()) == len(SCENARIOS)
    identities = {'plan_sha256': sha(OUT / 'perf-261-plan.json'), 'harness_sha256': sha(__file__),
                  'probe_sha256': sha(OUT / 'perf-261-probe.rs'), 'sides': {}}
    for side, path in SIDES.items():
        metadata = json.loads((EVIDENCE / ('setup-metadata-' + side + '.stdout')).read_text())
        for name in ['mango-agent-claude', 'mango-external-agents']:
            packages = [p for p in metadata['packages'] if p['name'] == name]
            assert len(packages) == 1 and str(SDK[side]) in packages[0]['manifest_path']
            assert packages[0]['source'] is None
        identities['sides'][side] = {
            'source_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=SDK[side], text=True).strip(),
            'dirty': subprocess.check_output(['git', 'status', '--porcelain'], cwd=SDK[side], text=True).strip(),
            'lock_sha256': sha(path / 'Cargo.lock'), 'manifest_sha256': sha(path / 'Cargo.toml'),
            'binary_sha256': sha(binary(side)), 'sdk_lock_sha256': sha(SDK[side] / 'Cargo.lock'),
            'sdk_manifest_sha256': sha(SDK[side] / 'Cargo.toml')}
        assert not identities['sides'][side]['dirty']
    assert identities['sides']['base']['sdk_lock_sha256'] == identities['sides']['variant']['sdk_lock_sha256']
    assert identities['sides']['base']['sdk_manifest_sha256'] == identities['sides']['variant']['sdk_manifest_sha256']
    (EVIDENCE / 'identities.json').write_text(json.dumps(identities, indent=2) + '\n')

def binary(side):
    return SIDES[side] / 'target/release/nested-text-allocation-probe'

def profile():
    identities = json.loads((EVIDENCE / 'identities.json').read_text())
    assert sha(__file__) == identities['harness_sha256']
    rows = []
    for pair, order in [(1, ['base', 'variant']), (2, ['variant', 'base'])]:
        for scenario in SCENARIOS:
            for position, side in enumerate(order, 1):
                assert sha(binary(side)) == identities['sides'][side]['binary_sha256']
                label = f'pair-{pair}-{scenario}-{position}-{side}'
                profile_path = EVIDENCE / (label + '.dhat.json')
                output = command(label, [str(binary(side)), 'profile', scenario, str(profile_path)],
                                 SIDES[side], 60)
                sample = json.loads(output)
                assert sample['scenario'] == scenario
                assert profile_path.exists()
                assert sample['released_blocks'] == sample['released_bytes'] == 0
                assert sample['finished_blocks'] == sample['finished_bytes'] == 0
                sample.update({'attempt': label, 'pair': pair, 'position': position, 'side': side,
                               'profile': profile_path.name, 'profile_sha256': sha(profile_path)})
                rows.append(sample)
                (EVIDENCE / 'raw-samples.json').write_text(json.dumps(rows, indent=2) + '\n')
    evaluation = []
    for pair in [1, 2]:
        for scenario in SCENARIOS:
            base = next(r for r in rows if r['pair'] == pair and r['scenario'] == scenario and r['side'] == 'base')
            variant = next(r for r in rows if r['pair'] == pair and r['scenario'] == scenario and r['side'] == 'variant')
            assert base['events'] == variant['events'] and base['closing_events'] == variant['closing_events']
            if scenario.startswith('ordinary'):
                passed = all(variant[k] <= base[k] for k in ['active_bytes', 'total_blocks', 'peak_bytes'])
            else:
                passed = variant['active_bytes'] <= base['active_bytes'] * 0.10
            evaluation.append({'pair': pair, 'scenario': scenario, 'passed': passed,
                               'base': base, 'variant': variant,
                               'active_delta_bytes': variant['active_bytes'] - base['active_bytes'],
                               'active_delta_percent': 100 * (variant['active_bytes'] / base['active_bytes'] - 1)})
    summary = {'valid': len(rows) == 24, 'accepted': all(r['passed'] for r in evaluation),
               'samples': len(rows), 'pairs_per_scenario': 2, 'evaluation': evaluation}
    (EVIDENCE / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps({'valid': summary['valid'], 'accepted': summary['accepted'], 'samples': len(rows)}), flush=True)

if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description='Reproduce the paired nested-text allocation experiment with its committed lock.')
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--verify-only', action='store_true', help='Validate source identities and stage the plan without building or profiling.')
    args = parser.parse_args()
    OUT = args.output.resolve()
    assert not OUT.exists(), 'Use a fresh output directory; preserve prior attempts.'
    ROOT = OUT / 'projects'
    EVIDENCE = OUT / 'results'
    AUDIT = EVIDENCE / 'attempts.jsonl'
    SIDES = {side: ROOT / side for side in ['base', 'variant']}
    SDK = {'base': args.baseline.resolve(), 'variant': args.candidate.resolve()}
    original_plan = Path(__file__).with_name('plan.json')
    plan = json.loads(original_plan.read_text())
    for side, key in [('base', 'baseline'), ('variant', 'candidate')]:
        actual = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=SDK[side], text=True).strip()
        assert actual == plan[key]['sha'], (side, actual, plan[key]['sha'])
        assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=SDK[side], text=True).strip()
        plan[key]['path'] = str(SDK[side])
    EVIDENCE.mkdir(parents=True)
    plan['created_at'] = stamp()
    plan['reproduction_of_plan_sha256'] = sha(original_plan)
    plan['evidence'] = str(EVIDENCE)
    (OUT / 'perf-261-plan.json').write_text(json.dumps(plan, indent=2) + '\n')
    shutil.copy2(Path(__file__).with_name('probe.rs'), OUT / 'perf-261-probe.rs')
    if args.verify_only:
        print('Exact clean source identities validated; plan/probe staged. No build or profile ran.')
    else:
        prepare()
        profile()
