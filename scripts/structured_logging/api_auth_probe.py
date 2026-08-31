#!/usr/bin/env python3
"""P3/15 auth boundary drill: every log entry, old and new, behind the login guard.

Starts the real server twice — once with `--auth`, once without — and probes the new event API
and the old file-based log stream. No account, no business data: a fresh working directory.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "data/observability-evidence"
# The old file stream is part of the boundary: it reads real log files off disk.
PATHS = [
    "/v1/log-events",
    "/v1/log-events/export",
    "/v1/log-events/stream",
    "/v1/log-events/2b5c6a4e-0000-4000-8000-000000000000/diagnostic",
    "/v1/ws/logs?file=ds_update",
]


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def probe(port, path, read_body=True):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request("GET", path)
    response = connection.getresponse()
    # A live stream never ends by design, so only its status line is read.
    body = response.read(4096) if read_body else b""
    connection.close()
    return response.status, body


def serve(work, port, auth, database):
    env = {k: v for k, v in os.environ.items() if not k.startswith("BILIUP_OBSERVABILITY")}
    env.update(
        RUST_LOG="info",
        BILIUP_OBSERVABILITY="1",
        BILIUP_OBSERVABILITY_INSTANCE="api-auth-probe",
        BILIUP_OBSERVABILITY_DB=str(database),
    )
    command = [str(ROOT / "target/debug/biliup"), "server", "--bind", "127.0.0.1",
               "--port", str(port)]
    if auth:
        command.append("--auth")
    stdout = (work / f"stdout-{'auth' if auth else 'open'}.txt").open("w")
    process = subprocess.Popen(command, cwd=work, env=env, stdout=stdout, stderr=subprocess.STDOUT)
    deadline = time.monotonic() + 30
    while True:
        if process.poll() is not None:
            raise AssertionError("server exited before it was ready")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return process, stdout
        except OSError:
            if time.monotonic() > deadline:
                raise TimeoutError("server startup")
            time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    out = args.output.resolve()
    if not out.is_relative_to(EVIDENCE.resolve()) or out.exists():
        raise ValueError("new private output directory required")
    out.mkdir(parents=True)
    report = {"guarded": {}, "open": {}}

    for mode, auth in [("guarded", True), ("open", False)]:
        work = out / mode
        work.mkdir()
        port = free_port()
        process, stdout = serve(work, port, auth, work / "events.sqlite")
        try:
            for path in PATHS:
                status, body = probe(port, path, read_body="/stream" not in path)
                entry = {"status": status}
                if mode == "open" and path == "/v1/log-events":
                    payload = json.loads(body)
                    entry["availability"] = payload["availability"]
                    entry["coverage"] = payload["coverage"]
                report[mode][path] = entry
        finally:
            process.terminate()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                # A held-open stream can keep graceful shutdown waiting; this drill is done.
                process.kill()
                process.wait(timeout=10)
            stdout.close()

    # With the guard on, nothing readable leaks — including the old file stream.
    for path, entry in report["guarded"].items():
        assert entry["status"] == 401, (path, entry)
    # With the guard off the deployment keeps its existing open semantics.
    assert report["open"]["/v1/log-events"]["status"] == 200, report["open"]
    assert report["open"]["/v1/log-events"]["availability"] == "ready"
    assert report["open"]["/v1/log-events"]["coverage"] == "native"
    assert report["open"]["/v1/log-events/export"]["status"] == 200
    # The old websocket entry answers a plain GET with 400 (upgrade required), not 401.
    assert report["open"]["/v1/ws/logs?file=ds_update"]["status"] != 401

    (out / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
