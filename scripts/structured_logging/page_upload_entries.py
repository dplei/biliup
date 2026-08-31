#!/usr/bin/env python3
"""P3/14 page upload HTTP correlation, opt-in and rollback on isolated Rust/wheel servers.

Build biliup and stream-gears first; use a Python matching the extension ABI. All credentials
are absent or malformed local fixtures, so no remote uploads/submissions are attempted.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import time
from urllib.parse import urlencode
import uuid

import evidence
import reconcile

ROOT = Path(__file__).resolve().parents[2]


def call(port, path, body=None):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    try:
        conn.request('GET' if body is None else 'POST', path,
                     body=None if body is None else json.dumps(body),
                     headers={'Content-Type': 'application/json'})
        response = conn.getresponse()
        data = response.read(1024 * 1024 + 1)
        assert len(data) <= 1024 * 1024, 'response cap exceeded'
        content_type = response.getheader('Content-Type', '')
        return response.status, (json.loads(data) if data and 'application/json' in content_type
                                 else data.decode('utf-8', errors='replace'))
    finally:
        conn.close()


def run(entry, work, mode):
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = {k: v for k, v in os.environ.items() if not k.startswith('BILIUP_OBSERVABILITY')}
    env.update(RUST_LOG='info', BILIUP_OBSERVABILITY='0' if mode == 'off' else '1',
               BILIUP_OBSERVABILITY_INSTANCE='page-upload-probe',
               BILIUP_OBSERVABILITY_DB=str(work / ('absent/events.sqlite' if mode == 'broken' else 'events.sqlite')))
    command = ([str(ROOT / 'target/debug/biliup'), 'server', '--bind', '127.0.0.1', '--port', str(port)]
               if entry == 'rust' else [sys.executable, str(ROOT / 'scripts/structured_logging/smoke_entries.py'),
                                       str(work), '--child', 'wheel-server', '--fixture', str(port)])
    with (work / 'stdout.txt').open('ab') as stdout, (work / 'stderr.txt').open('ab') as stderr:
        proc = subprocess.Popen(command, cwd=work, env=env, stdout=stdout, stderr=stderr)
        try:
            deadline = time.monotonic() + 25
            while True:
                assert proc.poll() is None, 'server exited before startup'
                try:
                    if call(port, '/v1/status')[0] == 200:
                        break
                except OSError:
                    pass
                assert time.monotonic() < deadline, 'server startup timeout'
                time.sleep(.1)
            # Only this new synthetic database is written; no worker/room is configured.
            with sqlite3.connect(work / 'data/data.sqlite3') as conn:
                conn.execute("INSERT OR IGNORE INTO streamerinfo (id,name,url,title,date,live_cover_path) "
                             "VALUES (1,'synthetic','https://example.invalid/live','synthetic',datetime('now'),'')")
                if not conn.execute('SELECT 1 FROM filelist').fetchone():
                    conn.execute("INSERT INTO filelist (file,streamer_info_id) VALUES ('known.flv',1)")
            (work / 'malformed.json').write_text('authorization=secret-sentinel')
            payloads = [{'files': files, 'params': {'id': 0, 'template_name': 'synthetic', 'tags': [],
                         'is_only_self': 1, 'user_cookie': str(work / cookie)}}
                        for files, cookie in [(['known.flv', 'other.flv'], 'absent.json'),
                                              (['other.flv'], 'malformed.json')] * 2]
            assert call(port, '/v1/uploads', {})[0] == 422
            with ThreadPoolExecutor(max_workers=4) as executor:
                responses = list(executor.map(lambda body: call(port, '/v1/uploads', body), payloads))
            tasks = []
            for index, (status, response) in enumerate(responses):
                assert status == 200, status
                assert response['matched'] == (index % 2 == 0), response
                assert response['streamer_name'] == ('synthetic' if index % 2 == 0 else None)
                uuid.UUID(response['task_id'])
                tasks.append(response['task_id'])
            assert len(set(tasks)) == 4
            if mode == 'on':
                deadline = time.monotonic() + 10
                for task in tasks:
                    query = urlencode({'assoc_key': 'task_id', 'assoc_value': task,
                                       'instance_id': 'page-upload-probe', 'order': 'asc'})
                    while True:
                        status, body = call(port, '/v1/log-events?' + query)
                        assert status == 200 and body['availability'] == 'ready'
                        rows = [e['data'] for e in body['events']]
                        if len(rows) == 2:
                            assert [e['fields']['values']['reason_code'] for e in rows] == [
                                'preparing_upload', 'authentication_failed']
                            assert rows[-1]['fields']['values']['outcome'] == 'failed'
                            break
                        assert time.monotonic() < deadline, 'background failure was not queryable'
                        time.sleep(.05)
            else:
                status, body = call(port, '/v1/log-events')
                assert status == 200
                assert body['availability'] == ('disabled' if mode == 'off' else 'unavailable')
            # Off/broken checks prove acceptance and server availability, not an unseen terminal
            # outcome. The regression test separately waits for the detached failure itself.
            assert call(port, '/v1/status')[0] == 200
            time.sleep(.3)  # ensure the graceful shutdown signal handler has been polled
            proc.send_signal(signal.SIGTERM)
            assert proc.wait(timeout=20) == 0
            return tasks
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    if not output.is_relative_to((ROOT / 'data/observability-evidence').resolve()) or output.exists():
        raise ValueError('use a new private evidence directory')
    output.mkdir(parents=True)
    paths = ['crates/biliup-cli/src/server/api/endpoints.rs', 'crates/biliup-cli/src/server/common/upload.rs',
             'crates/biliup-cli/src/observe.rs', 'crates/biliup-cli/src/observe/standalone.rs',
             'crates/biliup-observability/src/sanitize.rs',
             'scripts/structured_logging/page_upload_entries.py', 'scripts/structured_logging/evidence.py']
    fingerprint = hashlib.sha256(b''.join((ROOT / p).read_bytes() for p in paths)).hexdigest()
    report = []
    for entry in ['rust', 'wheel']:
        for mode in ['off', 'on', 'broken']:
            work = output / f'{entry}-{mode}'
            work.mkdir()
            tasks = run(entry, work, mode)
            database = work / 'events.sqlite'
            result = {'entry': entry, 'mode': mode, 'http_acceptance': 'passed', 'tasks': len(tasks)}
            logs = list(work.glob('*.log'))
            if entry == 'wheel':
                assert logs and sum(p.stat().st_size for p in logs) > 0
            if mode != 'on':
                assert not database.exists()
                if mode == 'broken':
                    assert 'observability_health=' in (work / 'stderr.txt').read_text()
                report.append(result)
                continue
            with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as conn:
                assert conn.execute('SELECT dirty,unclean_shutdowns FROM log_meta').fetchone() == (0, 0)
                native = [json.loads(r[0]) for r in conn.execute(
                    "SELECT payload FROM log_event WHERE capture_kind='native' ORDER BY id")]
            assert len(native) == 8
            assert {e['fields']['values']['task_id'] for e in native} == set(tasks)
            for event in native:
                assert event['event_name'] == 'submission.decided'
                assert not any(key in event['fields']['values'] for key in [
                    'segment_id', 'upload_session_id', 'streamer_info_id', 'live_streamer_id'])
            assert 'secret-sentinel' not in json.dumps(native)
            runs = []
            for line in (work / 'stderr.txt').read_text().splitlines():
                if line.startswith('observability_health='):
                    runs.extend(json.loads(line.split('=', 1)[1])['runs'])
            assert runs, 'missing shutdown health snapshot'
            legacy = logs + [work / 'stdout.txt', work / 'stderr.txt']
            request = {'database': str(database), 'since_ms': 0, 'until_ms': 9223372036854775807,
                       'source_version': fingerprint, 'grace_ms': 0,
                       'tasks': [{'task_id': task, 'sample': entry, 'state': 'finished'} for task in tasks],
                       'capture_config': {'enabled': True, 'bridge': True, 'native_range': ['upload', 'submission'],
                                          'new_filter': 'info', 'legacy_filter': 'info'},
                       'health': {'runs': runs, 'legacy_file_health': 'unknown'},
                       'legacy': [{'path': str(p), 'start': 0, 'end': p.stat().st_size, 'timezone': 'Asia/Shanghai',
                                   'kind': 'file' if p.suffix == '.log' else 'wrapper_process_output'} for p in legacy]}
            (work / 'request.json').write_text(json.dumps(request, indent=2))
            manifest = evidence.export(request, work / 'bundle')
            validation = evidence.validate(work / 'bundle')
            assert manifest['completeness']['status'] == 'complete', manifest['completeness']
            assert validation['status'] == 'passed', validation
            (work / 'validation.json').write_text(json.dumps(validation, indent=2))
            reconcile.prepare(work / 'bundle', work / 'views')
            before = database.read_bytes()
            old_size = sum(p.stat().st_size for p in legacy)
            run(entry, work, 'off')
            assert database.read_bytes() == before
            assert sum(p.stat().st_size for p in legacy) > old_size
            result.update(native_events=len(native), task_query='passed', bundle='complete',
                          validation='passed', rollback='passed')
            report.append(result)
    (output / 'report.json').write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
