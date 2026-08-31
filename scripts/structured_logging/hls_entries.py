#!/usr/bin/env python3
"""P3/14 HLS: real loopback media through Rust/wheel CLI and embedded Python.

The CLI's existing General extractor consumes a synthetic yt-dlp JSON response in a private
PATH. This proves the download entry after extraction, NOT yt-dlp/platform compatibility.
No accounts, external URLs, business database, uploads or background observation are used.
Build biliup-cli + stream-gears first; run with an extension-compatible Python interpreter.
"""
import argparse
import hashlib
import http.server
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
import uuid

import evidence
import reconcile

ROOT = Path(__file__).resolve().parents[2]
CASES = ['good-0', 'good-1', 'invalid', 'absent']


def embedded(entry, base):
    extension = ROOT / 'target/debug/libstream_gears.dylib'
    if not extension.exists():
        extension = ROOT / 'target/debug/libstream_gears.so'
    loader = importlib.machinery.ExtensionFileLoader('stream_gears', str(extension))
    spec = importlib.util.spec_from_loader('stream_gears', loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    callbacks = {}
    for case in CASES:
        url, output = f'{base}/{case}/index.m3u8', f'{case}-%s-%f'
        callbacks[case] = []

        class Callback:
            def __init__(self, path):
                callbacks[case].append(path)

        try:
            if entry == 'wheel':
                sys.argv = ['biliup', 'download', url, '-o', output]
                module.main_loop()
            else:
                module.download_with_callback(url, {}, output, module.PySegment(), Callback)
        except RuntimeError:
            assert case in {'invalid', 'absent'}, case
            print(f'fixture {case}: expected failure returned', flush=True)
        else:
            assert case.startswith('good'), case
    if entry == 'python':
        assert [len(callbacks[c]) for c in CASES] == [2, 2, 0, 1], callbacks
    Path('callbacks.json').write_text(json.dumps(callbacks, indent=2))
    Path('health.json').write_text(module.observability_health())


def run(entry, work, state, server, fixture_bin):
    env = {k: v for k, v in os.environ.items() if not k.startswith('BILIUP_OBSERVABILITY')}
    env.update(BILIUP_OBSERVABILITY='0' if state == 'off' else '1',
               BILIUP_OBSERVABILITY_INSTANCE='synthetic-hls', RUST_LOG='info',
               BILIUP_OBSERVABILITY_DB=str(work / ('missing/events.sqlite' if state == 'broken' else 'events.sqlite')),
               PATH=str(fixture_bin) + os.pathsep + env.get('PATH', ''))
    base = f'http://127.0.0.1:{server.server_port}/{uuid.uuid4().hex}'
    commands = [([str(ROOT / 'target/debug/biliup'), 'download', f'{base}/{case}/index.m3u8',
                  '-o', f'{case}-%s-%f'], case.startswith('good')) for case in CASES] if entry == 'rust' else [
        ([sys.executable, str(Path(__file__).resolve()), str(work), '--child', entry, '--base', base], True)]
    for command, success in commands:
        result = subprocess.run(command, cwd=work, env=env, capture_output=True, timeout=30)
        for name, data in [('stdout.txt', result.stdout), ('stderr.txt', result.stderr)]:
            with (work / name).open('ab') as handle:
                handle.write(data)
        assert (result.returncode == 0) == success, (entry, state, result.returncode, str(work))


def values(event):
    return event['fields']['values']


def verify_native(rows):
    native = [row for row in rows if row['capture_kind'] == 'native']
    assert all(row['fields']['quality']['rejected'] == 0 for row in native)
    named = lambda name: [row for row in native if row['event_name'] == name]
    started, stopped = named('recording.started'), named('recording.stopped')
    assert len(started) == len(stopped) == 4
    tasks = {values(row)['task_id'] for row in started}
    assert len(tasks) == 4 and {values(row)['task_id'] for row in stopped} == tasks
    attempts = {values(row)['task_id']: values(row)['download_attempt_id'] for row in started}
    assert len(set(attempts.values())) == 4
    assert sorted(values(row)['outcome'] for row in stopped) == ['executed', 'executed', 'failed', 'failed']
    for row in native:
        fields = values(row)
        assert fields['task_id'] in tasks
        assert not any(k in fields for k in ['live_streamer_id', 'streamer_info_id', 'upload_session_id'])
        if row['event_name'] != 'recording.stopped':
            assert fields['download_attempt_id'] == attempts[fields['task_id']]
    for task in tasks:
        assert len({r['process_run_id'] for r in native if values(r)['task_id'] == task}) == 1
    created, closed = named('recording.segment_created'), named('recording.segment_closed')
    files = {values(row)['segment_id']: values(row) for row in created}
    assert len(files) == len(created) == len(closed) == 5
    for row in closed:
        fields = values(row)
        assert fields['original_file'] == files[fields['segment_id']]['original_file']
        assert fields['task_id'] == files[fields['segment_id']]['task_id']
    for name in ['recording.hls_gap', 'recording.hls_discontinuity']:
        events = named(name)
        assert len(events) == 2
        for row in events:
            fields = values(row)
            assert fields['media_sequence'] == 3 and fields['segment_id'] in files
            assert fields['original_file'] == files[fields['segment_id']]['original_file']
            assert 'gap_ms' not in fields
            if name == 'recording.hls_gap':
                assert fields['previous_media_sequence'] == fields['missing_segments'] == 1
    assert sorted(values(r)['reason_code'] for r in named('recording.disconnected')) == ['http_error', 'invalid_playlist']
    assert not named('recording.reconnected'), 'standalone has no reconnect loop'
    return tasks, len(native)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--child', choices=['wheel', 'python'])
    parser.add_argument('--base')
    args = parser.parse_args()
    if args.child:
        embedded(args.child, args.base)
        return
    out = args.output.resolve()
    if not out.is_relative_to(ROOT / 'data/observability-evidence') or out.exists():
        raise ValueError('use a NEW ignored private evidence directory')
    out.mkdir(parents=True)
    fixture = out / 'synthetic.ts'
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i',
                    'testsrc=size=64x64:rate=5', '-t', '1', '-c:v', 'libx264', '-pix_fmt',
                    'yuv420p', '-f', 'mpegts', str(fixture)], check=True)
    media = fixture.read_bytes()
    fixture_bin = out / 'fixture-bin'
    fixture_bin.mkdir()
    extractor = fixture_bin / 'yt-dlp'
    extractor.write_text('#!' + sys.executable + '\nimport json, sys\nfrom urllib.parse import urlparse\n'
                         'url=sys.argv[-1]\nassert urlparse(url).hostname == "127.0.0.1"\n'
                         'print(json.dumps({"url":url,"manifest_url":url,"live_status":"is_live","title":"synthetic"}))\n')
    extractor.chmod(0o700)
    requests = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            key = self.path
            requests[key] = requests.get(key, 0) + 1
            status = 200
            if '/invalid/' in key:
                body = b'not a playlist'
            elif key.endswith('index.m3u8') and '/absent/' not in key:
                body = b'#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100000,RESOLUTION=64x64\nmedia.m3u8\n'
            elif '/absent/' in key and key.endswith('.ts'):
                status, body = 404, b'absent media'
            elif key.endswith('.ts'):
                body = media
            elif '/absent/' in key:
                body = b'#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nabsent.ts\n#EXT-X-ENDLIST\n'
            elif requests[key] == 1:
                body = b'#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\na.ts\n#EXTINF:1,\nb.ts\n'
            else:
                body = b'#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:3\n#EXT-X-DISCONTINUITY\n#EXTINF:1,\nc.ts\n#EXT-X-ENDLIST\n'
            self.send_response(status)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    sources = [ROOT / p for p in ['crates/biliup/src/downloader/hls.rs', 'crates/biliup/src/downloader/util.rs',
        'crates/biliup/src/downloader/error.rs', 'crates/biliup-cli/src/downloader.rs',
        'crates/biliup-cli/src/server/core/downloader/stream_gears.rs', 'crates/stream-gears/src/lib.rs',
        'scripts/structured_logging/hls_entries.py', 'scripts/structured_logging/evidence.py']]
    sources += sorted((ROOT / 'crates/biliup-observability/src').glob('*.rs'))
    fingerprint = hashlib.sha256(b''.join(p.read_bytes() for p in sources)).hexdigest()
    report = []
    try:
        for entry in ['rust', 'wheel', 'python']:
            for state in ['off', 'on', 'broken']:
                work = out / f'{entry}-{state}'
                work.mkdir()
                run(entry, work, state, server, fixture_bin)
                for case in CASES[:2]:
                    outputs = [p.read_bytes() for p in work.glob(f'{case}-*.ts')]
                    assert sorted(outputs, key=len) == [media, media + media], (entry, state, case)
                assert not list(work.glob('*.part')) and not (work / 'test.fmp4').exists()
                database = work / 'events.sqlite'
                stderr = (work / 'stderr.txt').read_text()
                old = list(work.glob('*.log')) or [work / 'stdout.txt']
                assert sum(p.stat().st_size for p in old) > 0, 'old source must continue'
                if state != 'on':
                    assert not database.exists()
                    if state == 'broken':
                        assert 'observability_health=' in stderr
                    report.append({'entry': entry, 'state': state, 'result': 'passed'})
                    continue
                with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True) as conn:
                    rows = [json.loads(row[0]) for row in conn.execute('SELECT payload FROM log_event ORDER BY id')]
                    assert conn.execute('SELECT dirty,unclean_shutdowns FROM log_meta').fetchone() == (0, 0)
                tasks, count = verify_native(rows)
                runs = []
                for line in stderr.splitlines():
                    if line.startswith('observability_health='):
                        runs.extend(json.loads(line.split('=', 1)[1])['runs'])
                request = {'database': str(database), 'since_ms': 0, 'until_ms': 9223372036854775807,
                    'source_version': fingerprint,
                    'tasks': [{'task_id': task, 'state': 'finished', 'sample': entry,
                               'scope': 'loopback HLS; synthetic CLI extractor'} for task in tasks],
                    'capture_config': {'enabled': True, 'bridge': True, 'native_range': ['recording'],
                                       'legacy_filter': 'info', 'new_filter': 'info'},
                    'health': {'runs': runs, 'legacy_file_health': 'unknown'}, 'grace_ms': 0,
                    'legacy': [{'path': str(p), 'start': 0, 'end': p.stat().st_size, 'timezone': 'Asia/Shanghai',
                                'kind': 'file' if p.suffix == '.log' else 'wrapper_stdout'} for p in old]}
                facts = [{'fact_id': name, 'event_name': name} for name in
                         ['recording.hls_gap', 'recording.hls_discontinuity', 'recording.disconnected']]
                (work / 'request.json').write_text(json.dumps(request, indent=2))
                (work / 'expectations.json').write_text(json.dumps(facts, indent=2))
                manifest = evidence.export(request, work / 'bundle')
                validation = evidence.validate(work / 'bundle', facts)
                (work / 'validation.json').write_text(json.dumps(validation, indent=2))
                assert manifest['completeness']['status'] == 'complete', manifest['completeness']
                assert validation['status'] == 'passed', validation
                reconcile.prepare(work / 'bundle', work / 'views')
                before, old_size = database.read_bytes(), sum(p.stat().st_size for p in old)
                run(entry, work, 'off', server, fixture_bin)
                assert database.read_bytes() == before
                assert sum(p.stat().st_size for p in old) > old_size
                report.append({'entry': entry, 'state': state, 'result': 'passed', 'tasks': len(tasks),
                               'native_events': count, 'bundle': 'complete', 'validation': 'passed', 'rollback': 'passed'})
        (out / 'report.json').write_text(json.dumps(report, indent=2))
        (out / 'requests.json').write_text(json.dumps(requests, indent=2))
        print(json.dumps(report, indent=2))
    finally:
        server.shutdown()
        server.server_close()


if __name__ == '__main__':
    main()
