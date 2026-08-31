#!/usr/bin/env python3
"""P3/12 controlled recording drill: synthetic media only, no account, no live platform.

Runs the real recording path (Python download function -> httpflv -> LifecycleFile) against a
local server that serves a synthetic FLV, and checks what the native events can answer:
two complete recordings, two interleaved recordings, continuous splitting, a transport failure,
and a hand-built DTS anomaly. Old files keep being written and are exported as the other source.
"""
import argparse
import hashlib
import http.server
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import struct
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / "data/observability-evidence"


def tags(data):
    """Yield (offset, tag_type, data_size, timestamp) for every FLV tag."""
    offset = 9 + 4
    while offset + 11 <= len(data):
        tag_type = data[offset]
        size = int.from_bytes(data[offset + 1 : offset + 4], "big")
        timestamp = int.from_bytes(data[offset + 4 : offset + 7], "big") | (data[offset + 7] << 24)
        yield offset, tag_type, size, timestamp
        offset += 11 + size + 4


def inject_dts_backward(source, target, jumps=3):
    """Rewrite a few non-keyframe video timestamps backwards, keeping every length intact."""
    data = bytearray(source.read_bytes())
    written = 0
    for offset, tag_type, size, timestamp in list(tags(bytes(data))):
        if written >= jumps or tag_type != 9 or timestamp < 400:
            continue
        frame_type = data[offset + 11] >> 4
        if frame_type == 1:  # never move a keyframe: splitting decisions read those timestamps
            continue
        lowered = max(timestamp - 300 - written * 100, 1)
        data[offset + 4 : offset + 7] = lowered.to_bytes(3, "big")
        data[offset + 7] = 0
        written += 1
    if written < jumps:
        raise SystemExit(f"fixture has too few movable video tags: {written}")
    target.write_bytes(bytes(data))
    return written


def serve(directory, truncate_after=None):
    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            body = (Path(directory) / self.path.lstrip("/")).read_bytes()
            cut = truncate_after if self.path.endswith("cut.flv") else None
            self.send_response(200)
            self.send_header("Content-Type", "video/x-flv")
            self.end_headers()
            try:
                # A cut mid-stream is a transport failure for the recorder, not a clean end.
                self.wfile.write(body[:cut] if cut else body)
            except (BrokenPipeError, ConnectionResetError):
                pass
            if cut:
                self.close_connection = True

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def child(work, urls, size_limit):
    extension = ROOT / "target/debug/libstream_gears.dylib"
    if not extension.exists():
        extension = ROOT / "target/debug/libstream_gears.so"
    loader = importlib.machinery.ExtensionFileLoader("stream_gears", str(extension))
    spec = importlib.util.spec_from_loader("stream_gears", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)

    def run(name, url):
        segment = module.PySegment()
        segment.size = size_limit
        callbacks = []

        class Callback:
            def __init__(self, path):
                callbacks.append(path)

        try:
            module.download_with_callback(url, {}, name, segment, Callback)
        except RuntimeError as error:  # a cut stream ends as a transport failure
            callbacks.append(f"error:{error}")
        return callbacks

    results = {}
    threads = []
    for name, url in urls["concurrent"].items():
        thread = threading.Thread(target=lambda n=name, u=url: results.update({n: run(n, u)}))
        threads.append(thread)
        thread.start()
    for thread in threads:
        thread.join()
    for name, url in urls["sequential"].items():
        results[name] = run(name, url)
    (work / "callbacks.json").write_text(json.dumps(results, indent=2))
    (work / "health.json").write_text(module.observability_health())


def events(database):
    with sqlite3.connect(database.as_uri() + "?mode=ro", uri=True) as conn:
        rows = [json.loads(row[0]) for row in conn.execute("SELECT payload FROM log_event ORDER BY id")]
        meta = conn.execute("SELECT dirty, unclean_shutdowns FROM log_meta").fetchone()
    return rows, meta


def native(rows, name):
    return [r for r in rows if r["capture_kind"] == "native" and r["event_name"] == name]


def field(row, key, default=None):
    return row["fields"]["values"].get(key, default)


def check(rows):
    """Every claim below is answered from the native events alone, never from old text."""
    report = {}
    created = native(rows, "recording.segment_created")
    closed = native(rows, "recording.segment_closed")
    enrolled = native(rows, "recording.segment_enrolled")
    dts = native(rows, "recording.dts_backward")
    disconnected = native(rows, "recording.disconnected")

    created_ids = [field(row, "segment_id") for row in created]
    assert created_ids and all(created_ids), "every created segment must carry an identity"
    assert len(set(created_ids)) == len(created_ids), "segment identities must be unique"
    report["segments_created"] = len(created)

    closed_ids = [field(row, "segment_id") for row in closed]
    assert set(closed_ids) <= set(created_ids), "a close can only name a segment that was created"
    files = {field(row, "segment_id"): field(row, "original_file") for row in created}
    for row in closed:
        assert field(row, "original_file") == files[field(row, "segment_id")], "close changed the file"
    report["segments_closed"] = len(closed)
    report["close_reasons"] = sorted({field(row, "reason_code") for row in closed})

    # Interleaving: a recording's identities must never appear under the other recording's name.
    by_recording = {}
    for segment_id, original in files.items():
        by_recording.setdefault(original.split("-part")[0], set()).add(segment_id)
    assert len(by_recording) >= 2, "the drill must produce more than one recording"
    for name, ids in by_recording.items():
        for other, other_ids in by_recording.items():
            if other != name:
                assert not ids & other_ids, "segment identity crossed between recordings"
    report["recordings"] = {name: len(ids) for name, ids in sorted(by_recording.items())}

    splits = [row for row in closed if field(row, "reason_code") == "split_limit"]
    assert len(splits) >= 2, "continuous splitting must be visible as split_limit closes"
    report["splits"] = len(splits)
    assert any(field(row, "reason_code") == "stream_end" for row in closed)

    pairs = [row for row in dts if "previous_ms" in row["fields"]["values"]]
    summaries = [row for row in dts if "count" in row["fields"]["values"]]
    assert pairs, "the injected DTS anomaly must be reported natively"
    for row in pairs:
        assert field(row, "previous_ms") > field(row, "current_ms"), "backward jump only"
        assert field(row, "segment_id") in created_ids
    report["dts_first"] = len(pairs)
    report["dts_summaries"] = [
        {"count": field(row, "count"), "max_backward_ms": field(row, "max_backward_ms")}
        for row in summaries
    ]

    failed = [row for row in disconnected if field(row, "outcome") == "failed"]
    assert failed, "the cut stream must produce a failed disconnect, not a clean end"
    report["disconnected"] = {
        "failed": len(failed),
        "succeeded": len([row for row in disconnected if field(row, "outcome") == "succeeded"]),
        "reasons": sorted({field(row, "reason_code") for row in disconnected}),
    }
    tasks = {field(row, "task_id") for row in created + closed + disconnected}
    assert tasks and all(tasks), "every recording event must name the task it belongs to"
    report["tasks"] = len(tasks)
    started = native(rows, "recording.started")
    stopped = native(rows, "recording.stopped")
    assert len(started) == len(stopped) == len(tasks), "each recording starts and stops once"
    report["stopped_reasons"] = sorted({field(row, "reason_code") for row in stopped})
    report["enrolled"] = len(enrolled)  # the Python entry has no upload ledger; expected zero
    report["bridge_events"] = len([r for r in rows if r["capture_kind"] == "legacy_bridge"])
    report["native_events"] = len([r for r in rows if r["capture_kind"] == "native"])
    return report


def expectations(rows):
    """Facts this drill claims, written so they survive the exporter's identity anonymization.

    Business identities are aliased per batch, so an expectation can never quote a raw id. The
    identity chain itself is checked twice: on the raw database by `check`, and inside the bundle
    by the exporter's owner-conflict rule, which works on the aliases.
    """
    del rows
    return [
        {"fact_id": "C03-created", "event_name": "recording.segment_created",
         "fields": {"outcome": "executed"}},
        {"fact_id": "C03-split", "event_name": "recording.segment_closed",
         "fields": {"reason_code": "split_limit"}},
        {"fact_id": "C03-stream-end", "event_name": "recording.segment_closed",
         "fields": {"reason_code": "stream_end"}},
        {"fact_id": "C04-dts-first", "event_name": "recording.dts_backward",
         "fields": {"reason_code": "timestamp_backward"}},
        {"fact_id": "C05-transport", "event_name": "recording.disconnected",
         "fields": {"outcome": "failed", "reason_code": "transport_error"}},
        {"fact_id": "C02-started", "event_name": "recording.started",
         "fields": {"reason_code": "live_detected"}},
        {"fact_id": "C02-stopped-failed", "event_name": "recording.stopped",
         "fields": {"outcome": "failed", "reason_code": "transport_error"}},
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--child", action="store_true")
    parser.add_argument("--urls")
    parser.add_argument("--size-limit", type=int, default=25_000)
    args = parser.parse_args()
    if args.child:
        child(args.output.resolve(), json.loads(args.urls), args.size_limit)
        return

    out = args.output.resolve()
    if not out.is_relative_to(EVIDENCE.resolve()) or out.exists():
        raise ValueError("new private output directory required")
    out.mkdir(parents=True)
    media = out / "media"
    media.mkdir()
    base = media / "base.flv"
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "lavfi",
         "-i", "testsrc=size=320x240:rate=15", "-f", "lavfi", "-i", "sine=frequency=440",
         "-t", "6", "-c:v", "libx264", "-g", "15", "-pix_fmt", "yuv420p",
         "-c:a", "aac", "-f", "flv", str(base)],
        check=True,
    )
    jumps = inject_dts_backward(base, media / "dts.flv")
    shutil.copyfile(media / "dts.flv", media / "cut.flv")
    server = serve(media, truncate_after=base.stat().st_size // 3)
    port = server.server_port
    urls = {
        "concurrent": {
            "room-a-part": f"http://127.0.0.1:{port}/dts.flv",
            "room-b-part": f"http://127.0.0.1:{port}/dts.flv",
        },
        "sequential": {"room-c-part": f"http://127.0.0.1:{port}/cut.flv"},
    }

    work = out / "run"
    work.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith("BILIUP_OBSERVABILITY")}
    database = work / "events.sqlite"
    env.update(
        RUST_LOG="info",
        BILIUP_OBSERVABILITY="1",
        BILIUP_OBSERVABILITY_INSTANCE="recording-pilot",
        BILIUP_OBSERVABILITY_DB=str(database),
    )
    command = [sys.executable, __file__, str(work), "--child", "--urls", json.dumps(urls),
               "--size-limit", str(args.size_limit)]
    with (work / "stdout.txt").open("w") as stdout, (work / "stderr.txt").open("w") as stderr:
        code = subprocess.run(command, cwd=work, env=env, stdout=stdout, stderr=stderr,
                              timeout=300).returncode
    server.shutdown()
    server.server_close()
    assert code == 0, f"pilot child failed: {code}"

    rows, meta = events(database)
    assert meta == (0, 0), f"storage health must be clean: {meta}"
    report = check(rows)
    report["dts_injected"] = jumps
    logs = sorted(work.glob("*.log"))
    assert logs, "the old download log must still be written"
    report["old_log_bytes"] = sum(path.stat().st_size for path in logs)

    sources = [ROOT / "crates/biliup/src/downloader/util.rs",
               ROOT / "crates/biliup/src/downloader/httpflv.rs",
               ROOT / "crates/biliup-cli/src/observe.rs",
               *sorted((ROOT / "crates/biliup-observability/src").glob("*.rs"))]
    fingerprint = hashlib.sha256(b"".join(p.read_bytes() for p in sources)).hexdigest()
    health = []
    for line in (work / "stderr.txt").read_text().splitlines():
        if line.startswith("observability_health="):
            health.extend(json.loads(line.split("=", 1)[1])["runs"])
    request = {
        "database": str(database),
        "since_ms": 0,
        "until_ms": 9223372036854775807,
        "source_version": fingerprint,
        "display_timezone": "Asia/Shanghai",
        "tasks": [{"sample": "recording-pilot", "state": "finished",
                   "scope": "controlled synthetic media, no account"}],
        "capture_config": {"enabled": True, "bridge": True,
                           "native_range": ["recording"], "legacy_filter": "info",
                           "new_filter": "info"},
        "health": {"runs": health, "legacy_file_health": "unknown"},
        "grace_ms": 0,
        "legacy": [{"path": str(path), "start": 0, "end": path.stat().st_size,
                    "timezone": "Asia/Shanghai"} for path in logs],
    }
    (out / "request.json").write_text(json.dumps(request, indent=2))
    (out / "expectations.json").write_text(json.dumps(expectations(rows), indent=2))
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
