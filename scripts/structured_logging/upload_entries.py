#!/usr/bin/env python3
"""P3/14 standalone upload entry errors, opt-in and rollback; no remote account operations.

Build biliup and stream-gears first. Run with a Python version matching the extension ABI.
Success/transfer tests are intentionally separate: missing credentials never reach the network.
"""
import argparse
import hashlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

import evidence

ROOT = Path(__file__).resolve().parents[2]


def embedded(entry):
    extension = ROOT / 'target/debug/libstream_gears.dylib'
    if not extension.exists():
        extension = ROOT / 'target/debug/libstream_gears.so'
    loader = importlib.machinery.ExtensionFileLoader('stream_gears', str(extension))
    spec = importlib.util.spec_from_loader('stream_gears', loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    commands = [['upload', 'input.flv'], ['upload', '--config', 'absent.toml'],
                ['append', '--vid', 'av1', 'input.flv']] if entry == 'wheel' else [None]
    for command in commands * 2:
        try:
            if entry == 'wheel':
                sys.argv = ['biliup', '--user-cookie', 'absent.json', *command]
                module.main_loop()
            else:
                module.upload([Path('input.flv')], Path('absent.json'), 'synthetic')
        except RuntimeError:
            print('synthetic invocation: expected local credential failure', flush=True)
        else:
            raise AssertionError('missing credential unexpectedly succeeded')
    Path('health.json').write_text(module.observability_health())


def run(entry, work, state):
    env = {k: v for k, v in os.environ.items() if not k.startswith('BILIUP_OBSERVABILITY')}
    env.update(BILIUP_OBSERVABILITY='0' if state == 'off' else '1',
               BILIUP_OBSERVABILITY_INSTANCE='synthetic',
               BILIUP_OBSERVABILITY_DB=str(work / ('missing/events.sqlite' if state == 'broken' else 'events.sqlite')),
               RUST_LOG='info')
    cmd = ([str(ROOT / 'target/debug/biliup'), '--user-cookie', 'absent.json', 'upload', 'input.flv']
           if entry == 'rust' else [sys.executable, str(Path(__file__).resolve()), str(work), '--child', entry])
    commands = ([cmd, [*cmd[:3], 'upload', '--config', 'absent.toml'],
                 [*cmd[:3], 'append', '--vid', 'av1', 'input.flv']] * 2 if entry == 'rust' else [cmd])
    for command in commands:
        result = subprocess.run(command, cwd=work, env=env, capture_output=True, timeout=30)
        assert (result.returncode != 0) if entry == 'rust' else (result.returncode == 0), (entry, state)
        for name, data in [('stdout.txt', result.stdout), ('stderr.txt', result.stderr)]:
            with (work / name).open('ab') as f:
                f.write(data)
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--child', choices=['wheel', 'python'])
    args = parser.parse_args()
    if args.child:
        embedded(args.child)
        return
    out = args.output.resolve()
    if not out.is_relative_to(ROOT / 'data/observability-evidence') or out.exists():
        raise ValueError('use a new private evidence directory')
    out.mkdir(parents=True)
    report = []
    sources = [ROOT / path for path in [
        'crates/biliup-cli/src/uploader.rs', 'crates/biliup-cli/src/observe.rs',
        'crates/biliup-cli/src/observe/standalone.rs', 'crates/biliup-cli/src/server/common/upload.rs',
        'crates/stream-gears/src/uploader.rs', 'scripts/structured_logging/evidence.py']]
    fingerprint = hashlib.sha256(b''.join(p.read_bytes() for p in sources)).hexdigest()
    for entry in ['rust', 'wheel', 'python']:
        for state in ['off', 'on', 'broken']:
            work = out / f'{entry}-{state}'
            work.mkdir()
            run(entry, work, state)
            database = work / 'events.sqlite'
            stderr = (work / 'stderr.txt').read_text()
            if state != 'on':
                assert not database.exists()
                if state == 'broken':
                    assert 'observability_health=' in stderr
                report.append({'entry': entry, 'capture': state, 'result': 'passed'})
                continue
            with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as conn:
                events = [json.loads(row[0]) for row in conn.execute('SELECT payload FROM log_event ORDER BY id')]
                assert conn.execute('SELECT dirty,unclean_shutdowns FROM log_meta').fetchone() == (0, 0)
            native = [e for e in events if e['capture_kind'] == 'native']
            tasks = {}
            for event in native:
                fields = event['fields']['values']
                assert event['event_name'] == 'submission.decided'
                assert not any(k in fields for k in ['segment_id', 'live_streamer_id', 'streamer_info_id', 'upload_session_id'])
                tasks.setdefault(fields['task_id'], []).append(fields['reason_code'])
            assert len(tasks) == (2 if entry == 'python' else 6)
            assert all(chain == ['preparing_upload', 'authentication_failed'] for chain in tasks.values())
            for task in tasks:
                assert len({e['process_run_id'] for e in native if e['fields']['values']['task_id'] == task}) == 1
            # Sequential calls drop their last guard and create a fresh run. Sharing is only
            # required while guards overlap, covered by the independent shadow tests.
            runs = []
            for line in stderr.splitlines():
                if line.startswith('observability_health='):
                    runs.extend(json.loads(line.split('=', 1)[1])['runs'])
            # Old sinks may legitimately contain no event before this failure. The real captured
            # stderr is supplied independently as process evidence, never reconstructed from SQL.
            legacy = list(work.glob('*.log')) + [work / 'stdout.txt', work / 'stderr.txt']
            request = {
                'database': str(database), 'since_ms': 0, 'until_ms': 9223372036854775807,
                'source_version': fingerprint,
                'tasks': [{'task_id': task, 'state': 'finished', 'sample': entry} for task in tasks],
                'capture_config': {'enabled': True, 'bridge': True, 'native_range': ['upload', 'submission'],
                                   'legacy_filter': 'info', 'new_filter': 'info'},
                'health': {'runs': runs, 'legacy_file_health': 'unknown'},
                'legacy': [{'path': str(p), 'start': 0, 'end': p.stat().st_size, 'timezone': 'Asia/Shanghai',
                            'kind': 'file' if p.suffix == '.log' else 'wrapper_process_output'} for p in legacy],
                'grace_ms': 0,
            }
            (work / 'request.json').write_text(json.dumps(request, indent=2))
            expectations = [{'fact_id': 'local-failure', 'event_name': 'submission.decided',
                             'fields': {'outcome': 'failed', 'reason_code': 'authentication_failed'}}]
            (work / 'expectations.json').write_text(json.dumps(expectations, indent=2))
            manifest = evidence.export(request, work / 'bundle')
            validation = evidence.validate(work / 'bundle', expectations)
            assert manifest['completeness']['status'] == 'complete', manifest['completeness']
            assert validation['status'] == 'passed', validation
            (work / 'validation.json').write_text(json.dumps(validation, indent=2))
            # A fresh invocation with collection off must not alter the already committed DB.
            before = database.read_bytes()
            previous_output = (work / 'stderr.txt').stat().st_size + (work / 'stdout.txt').stat().st_size
            run(entry, work, 'off')
            assert database.read_bytes() == before
            assert (work / 'stderr.txt').stat().st_size + (work / 'stdout.txt').stat().st_size > previous_output
            report.append({'entry': entry, 'capture': state, 'result': 'passed',
                           'distinct_tasks': len(tasks), 'native_events': len(native),
                           'bundle': 'complete', 'validation': 'passed', 'rollback': 'passed'})
    (out / 'report.json').write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
