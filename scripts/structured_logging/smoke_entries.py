#!/usr/bin/env python3
"""Manual P2 entry smoke: loopback synthetic media, empty business DB, no account/network actions."""
import argparse
import hashlib
import http.server
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
from functools import partial

ROOT = Path(__file__).resolve().parents[2]


def child(extension, mode, fixture):
    loader = importlib.machinery.ExtensionFileLoader('stream_gears', str(extension))
    spec = importlib.util.spec_from_loader('stream_gears', loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    if mode == 'wheel-server':
        sys.argv = ['biliup', 'server', '--bind', '127.0.0.1', '--port', fixture]
        mod.main_loop()
    elif mode == 'wheel':
        for index in range(2):
            path=Path(f'input-{index}.flv'); shutil.copyfile(fixture,path)
            sys.argv=['biliup','dump-flv',str(path)]
            mod.main_loop()
    elif mode == 'python':
        segment=mod.PySegment()
        callbacks=[]
        class Callback:
            def __init__(self, path):
                callbacks.append(path)
        for index in range(2):
            mod.download_with_callback(fixture, {}, f'segment-{index}', segment, Callback)
        assert len(callbacks) >= 2
        # Explicitly missing local credentials returns before any authenticated remote operation.
        for _ in range(2):
            try:
                mod.upload([], Path('absent-cookies.json'), 'synthetic')
            except RuntimeError:
                pass
            else:
                raise AssertionError('missing credential should fail')
            try:
                mod.login_by_cookies('absent-cookies.json', None)
            except RuntimeError:
                pass
            else:
                raise AssertionError('missing credential should fail')
    Path('health.json').write_text(mod.observability_health())


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output',type=Path)
    parser.add_argument('--child',choices=['wheel','wheel-server','python'])
    parser.add_argument('--fixture')
    args=parser.parse_args()
    extension=ROOT/'target/debug/libstream_gears.dylib'
    if not extension.exists():
        extension=ROOT/'target/debug/libstream_gears.so'
    if args.child:
        child(extension,args.child,args.fixture)
        return
    out=args.output.resolve()
    if not out.is_relative_to((ROOT/'data/observability-evidence').resolve()) or out.exists():
        raise ValueError('new private output directory required')
    out.mkdir(parents=True)
    fixture=out/'synthetic.flv'
    subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-f','lavfi','-i','testsrc=size=64x64:rate=5','-t','1','-c:v','libx264','-pix_fmt','yuv420p','-f','flv',str(fixture)],check=True)
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self,*args):
            pass
    httpd=http.server.ThreadingHTTPServer(('127.0.0.1',0),partial(Quiet,directory=str(out)))
    threading.Thread(target=httpd.serve_forever,daemon=True).start()
    url=f'http://127.0.0.1:{httpd.server_port}/synthetic.flv'
    report=[]
    try:
        for entry in ['rust','wheel','python','rust-server','wheel-server']:
            for state in ['off','on','broken']:
                work=out/f'{entry}-{state}'; work.mkdir()
                env=os.environ.copy()
                for key in list(env):
                    if key.startswith('BILIUP_OBSERVABILITY'):
                        del env[key]
                env.update(RUST_LOG='info',BILIUP_OBSERVABILITY='0' if state=='off' else '1',BILIUP_OBSERVABILITY_INSTANCE='synthetic',BILIUP_OBSERVABILITY_DB=str(work/('absent/events.sqlite' if state=='broken' else 'events.sqlite')))
                if entry == 'rust':
                    shutil.copyfile(fixture,work/'input.flv')
                    cmd=[str(ROOT/'target/debug/biliup'),'dump-flv','input.flv']
                elif entry=='wheel':
                    cmd=[sys.executable,__file__,str(work),'--child','wheel','--fixture',str(fixture)]
                elif entry=='python':
                    cmd=[sys.executable,__file__,str(work),'--child','python','--fixture',url]
                else:
                    with socket.socket() as sock:
                        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
                    cmd=([str(ROOT/'target/debug/biliup'),'server','--bind','127.0.0.1','--port',str(port)] if entry=='rust-server' else [sys.executable,__file__,str(work),'--child','wheel-server','--fixture',str(port)])
                with (work/'stdout.txt').open('w') as stdout,(work/'stderr.txt').open('w') as stderr:
                    proc=subprocess.Popen(cmd,cwd=work,env=env,stdout=stdout,stderr=stderr)
                    try:
                        if entry.endswith('server'):
                            deadline=time.monotonic()+20
                            while True:
                                if proc.poll() is not None:
                                    raise AssertionError('server exited before ready: '+str(work))
                                try:
                                    with socket.create_connection(('127.0.0.1',port),timeout=.2):
                                        break
                                except OSError:
                                    if time.monotonic()>deadline:
                                        raise TimeoutError('server startup')
                                    time.sleep(.1)
                            time.sleep(.3)  # allow the graceful shutdown future to install handlers
                            proc.send_signal(signal.SIGTERM)
                        code=proc.wait(timeout=30)
                        assert code==0,(entry,state,code,str(work))
                    finally:
                        if proc.poll() is None:
                            proc.send_signal(signal.SIGTERM)
                            proc.wait(timeout=10)
                events=[]
                database=work/'events.sqlite'
                if state=='on':
                    with sqlite3.connect(database.as_uri()+'?mode=ro',uri=True) as conn:
                        events=[json.loads(r[0]) for r in conn.execute('SELECT payload FROM log_event')]
                        assert conn.execute('SELECT dirty,unclean_shutdowns FROM log_meta').fetchone()==(0,0)
                    assert events,(entry,'no captured events')
                    assert all(r['capture_kind']=='legacy_bridge' for r in events)
                else:
                    assert not database.exists()
                logs=list(work.glob('*.log'))
                if entry != 'rust' and entry != 'rust-server':
                    assert logs and sum(p.stat().st_size for p in logs)>0,(entry,state,'old logs missing')
                if state=='broken':
                    assert 'observability_health=' in (work/'stderr.txt').read_text()
                if state == 'on':
                    health_runs=[]
                    for line in (work/'stderr.txt').read_text().splitlines():
                        if line.startswith('observability_health='):
                            health_runs.extend(json.loads(line.split('=',1)[1])['runs'])
                    sources=[ROOT/'crates/biliup-cli/src/main.rs',ROOT/'crates/stream-gears/src/lib.rs',ROOT/'crates/stream-gears/src/server.rs',*sorted((ROOT/'crates/biliup-observability/src').glob('*.rs'))]
                    fingerprint=hashlib.sha256(b''.join(p.read_bytes() for p in sources)).hexdigest()
                    request={'database':str(database),'since_ms':0,'until_ms':9223372036854775807,'source_version':fingerprint,'tasks':[{'sample':entry,'state':'finished','scope':'controlled isolated process'}],'capture_config':{'enabled':True,'bridge':True,'native_range':[],'legacy_filter':'tower_http=debug,info' if entry.startswith('rust') else 'info','new_filter':'info'},'health':{'runs':health_runs,'legacy_file_health':'unknown'},'grace_ms':0,'legacy':[{'path':str(p),'start':0,'end':p.stat().st_size,'timezone':'Asia/Shanghai'} for p in logs]}
                    # Rust CLI has no persistent old sink. Its wrapper-captured stdout is a real
                    # independent source, explicitly selected only for this controlled exercise.
                    if not logs:
                        request['legacy']=[{'path':str(work/'stdout.txt'),'start':0,'end':(work/'stdout.txt').stat().st_size,'timezone':'Asia/Shanghai','kind':'wrapper_stdout'}]
                    (work/'request.json').write_text(json.dumps(request,indent=2))
                report.append({'entry':entry,'capture':state,'exit':code,'events':len(events),'old_files':len(logs)})
        # On -> off is an actual restart; an existing event DB stays unchanged while old files append.
        work=out/'wheel-on'
        before=(work/'events.sqlite').read_bytes()
        before_old=sum(p.stat().st_size for p in work.glob('*.log'))
        (work/'input-0.flv.json').unlink(); (work/'input-1.flv.json').unlink()
        env=os.environ.copy(); env['BILIUP_OBSERVABILITY']='0'; env['RUST_LOG']='info'
        result=subprocess.run([sys.executable,__file__,str(work),'--child','wheel','--fixture',str(fixture)],cwd=work,env=env,capture_output=True,timeout=30)
        assert result.returncode==0
        assert (work/'events.sqlite').read_bytes()==before
        assert sum(p.stat().st_size for p in work.glob('*.log'))>before_old
        report.append({'entry':'wheel','capture':'rollback-off','database_unchanged':True,'old_appended':True})
        (out/'report.json').write_text(json.dumps(report,indent=2))
        print(json.dumps(report,indent=2))
    finally:
        httpd.shutdown()
        httpd.server_close()


if __name__=='__main__':
    main()
