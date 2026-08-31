#!/usr/bin/env python3
"""Bounded, local, read-only evidence export. Inputs and log text are data, never commands."""
import argparse
from contextlib import closing
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import secrets
import sqlite3
import time

VERSION = "evidence-v1"
ROOT = Path(__file__).resolve().parents[2]
MAX_BYTES = 16 * 1024 * 1024
MAX_ROWS = 20000
MAX_SECONDS = 10
IDS = {"instance_id", "process_run_id", "event_uid", "live_streamer_id", "streamer_info_id", "upload_session_id", "segment_id", "missing_id", "download_attempt_id", "upload_attempt_id", "task_id", "streamer_name", "original_file", "artifact_file", "history_row_id"}
SENSITIVE = re.compile(r"cookie|authorization|token|secret|password|credential|bearer|signature|sign=|https?:|://|access_key|api_key|sessdata|bili_jct", re.I)
CATEGORIES = {"system", "recording", "processing", "upload", "submission", "auth", "audit"}
# Zone names are controlled identifiers, not free text: keep them readable, reject anything else.
ZONE = re.compile(r"^(?:UTC|Z|[+-]\d{2}:\d{2}|[A-Za-z][A-Za-z_+-]{0,20}(?:/[A-Za-z][A-Za-z0-9_+-]{0,20}){0,2})$")


def zone(value):
    value = str(value)
    return value if ZONE.match(value) else "unknown"

OUTCOMES = {"executed", "skipped", "fallback", "failed", "waiting", "succeeded", "unknown", "recovered", "cancelled"}
# Catalog requirements used only for native evidence. No parsing text into business identity.
REQUIRED = {
 "recording.dts_backward": ["segment_id", "previous_ms", "current_ms"],
 "recording.segment_created": ["segment_id", "original_file"],
 "recording.segment_closed": ["segment_id", "original_file", "size_bytes", "reason_code"],
 "recording.segment_enrolled": ["segment_id", "original_file"],
 "processing.decided": ["segment_id", "original_file", "reason_code", "outcome"],
 "processing.completed": ["segment_id", "original_file", "reason_code", "outcome"],
 "upload.started": ["segment_id", "upload_attempt_id"],
 "upload.failed": ["segment_id", "upload_attempt_id", "outcome", "reason_code"],
 "upload.completed": ["segment_id", "upload_attempt_id", "outcome"],
 "submission.completed": ["outcome", "reason_code"],
}


def read_json(path, cap=1024 * 1024):
    with open(path, "rb") as f:
        data = f.read(cap + 1)
    if len(data) > cap:
        raise ValueError("input_limit")
    return json.loads(data)


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n").encode()


class Incomplete(Exception):
    pass


class Budget:
    def __init__(self):
        self.deadline = time.monotonic() + MAX_SECONDS
        self.bytes = 0
        self.rows = 0
        self.input_bytes = 0
        self.query_ms = []

    def read_input(self, size):
        self.input_bytes += size
        if self.input_bytes > 32 * 1024 * 1024:
            raise Incomplete("source_byte_limit")
        self.check()

    def check(self, size=0):
        if time.monotonic() > self.deadline:
            raise Incomplete("export_deadline")
        if self.bytes + size > MAX_BYTES:
            raise Incomplete("export_byte_limit")
        self.bytes += size


class Anonymizer:
    def __init__(self):
        self.key = secrets.token_bytes(32)
        self.mapping = {}
        self.text_pattern = None
        self.text_values = {}

    def alias(self, kind, value):
        raw = str(value)
        if (kind, raw) not in self.mapping:
            digest = hmac.new(self.key, (kind + "\0" + raw).encode(), hashlib.sha256).hexdigest()[:16]
            self.mapping[kind, raw] = kind + "-" + digest
            if kind not in {"event_uid", "process_run_id", "instance_id"}:
                self.text_pattern = None
        return self.mapping[kind, raw]

    def text(self, value):
        # Omit the whole overlong value: a secret marker may follow the retained prefix.
        if len(value.encode()) > 8192:
            return "[OMITTED:oversize]"
        if SENSITIVE.search(value):
            return "[REDACTED]"
        # Compile once after the bounded identity prepass; never quadratic scans per message.
        if self.text_pattern is None:
            candidates = {}
            for (kind, raw), alias in self.mapping.items():
                if raw and kind not in {"event_uid", "process_run_id", "instance_id"}:
                    candidates.setdefault(raw, set()).add(alias)
            self.text_values = {raw: next(iter(values)) if len(values) == 1 else "[AMBIGUOUS_ID]" for raw, values in candidates.items()}
            pattern = "|".join(re.escape(raw) for raw in sorted(candidates, key=len, reverse=True))
            self.text_pattern = re.compile(r"(?<![\w])(?:" + pattern + r")(?![\w])") if pattern else re.compile(r"(?!x)x")
        value = self.text_pattern.sub(lambda m: self.text_values[m.group()], value)
        value = re.sub(r"(?:/[^\s,\"']+)+|[A-Za-z]:\\[^\s]+", "[PATH]", value)
        value = re.sub(r"\b(?:BV[0-9A-Za-z]{10}|(?:uid|aid|room_id)\s*[=:]\s*\d+)\b", "[ACCOUNT]", value, flags=re.I)
        return "".join(c if c >= " " else " " for c in value)

    def seed(self, obj):
        if isinstance(obj, dict):
            for key, value in obj.items():
                if key in IDS and isinstance(value, (str, int)):
                    self.alias(key, value)
                else:
                    self.seed(value)
        elif isinstance(obj, list):
            for value in obj:
                self.seed(value)

    def clean(self, obj, key=""):
        if isinstance(obj, dict):
            return {k: ("[REDACTED]" if SENSITIVE.search(k) else self.clean(v, k)) for k, v in obj.items()}
        if isinstance(obj, list):
            return [self.clean(v, key) for v in obj]
        if key in IDS and isinstance(obj, (str, int)):
            return self.alias(key, obj)
        if isinstance(obj, str):
            if key in {"ref", "file_ref", "fact_id", "event_name", "category", "level", "capture_kind", "reason_code", "sha256", "raw_sha256", "timezone", "display_timezone"}:
                return obj
            return self.text(obj)
        return obj


def readonly(path, budget):
    # mode=ro, not immutable: include committed WAL and never migrate/create the source.
    conn = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True, timeout=.05)
    conn.execute("PRAGMA query_only=ON")
    conn.execute("PRAGMA trusted_schema=OFF")
    # Each query also has its own VM deadline, in addition to the total export deadline.
    conn.set_progress_handler(lambda: time.monotonic() > budget.deadline, 1000)
    return conn


def query(conn, sql, args, budget):
    budget.check()
    started = time.monotonic()
    conn.set_progress_handler(lambda: time.monotonic() > min(budget.deadline, started + .2), 1000)
    try:
        rows = conn.execute(sql, args).fetchall()
    finally:
        budget.query_ms.append((time.monotonic() - started) * 1000)
    return rows


def generation(stat):
    return {"device": stat.st_dev, "inode": stat.st_ino, "size": stat.st_size, "mtime_ns": stat.st_mtime_ns, "ctime_ns": stat.st_ctime_ns}


class Bundle:
    def __init__(self, out, budget, anon):
        self.out, self.budget, self.anon = out, budget, anon
        self.files = {}

    def write(self, name, obj):
        blob = encoded(self.anon.clean(obj))
        self.budget.check(len(blob))
        with (self.out / name).open("ab") as f:
            f.write(blob)
        self.files[name] = self.files.get(name, 0) + 1

    def hashes(self):
        return {name: {"records": count, "sha256": hashlib.sha256((self.out / name).read_bytes()).hexdigest()} for name, count in self.files.items()}


def export(request, out):
    out = Path(out).resolve()
    # Production evidence cannot accidentally be published in scratch/source directories.
    private_root = (ROOT / "data" / "observability-evidence").resolve()
    if not out.is_relative_to(private_root) or out == private_root:
        raise ValueError("output_must_be_private_evidence_subdirectory")
    if out.exists():
        raise ValueError("output_exists_use_new_supplement_directory")
    if not isinstance(request.get("tasks"), list) or not request["tasks"]:
        raise ValueError("explicit_task_inventory_required")
    if len(request["tasks"]) > 200 or len(request.get("legacy", [])) > 32:
        raise ValueError("inventory_limit")
    since, until = request["since_ms"], request["until_ms"]
    if type(since) is not int or type(until) is not int or since > until:
        raise ValueError("invalid_utc_range")
    out.mkdir(parents=True, mode=0o700)
    budget, anon = Budget(), Anonymizer()
    if request.get("mapping_from"):
        mapping_path = Path(request["mapping_from"]).resolve()
        if mapping_path.name != ".private-map.json" or not mapping_path.is_relative_to(private_root):
            raise ValueError("mapping_must_remain_private")
        for item in read_json(mapping_path, MAX_BYTES):
            if item["kind"] not in IDS or not all(isinstance(item[k], str) for k in ("raw", "alias")):
                raise ValueError("invalid_private_mapping")
            anon.mapping[item["kind"], item["raw"]] = item["alias"]
    anon.seed(request.get("entities", {}))
    anon.seed(request["tasks"])
    bundle = Bundle(out, budget, anon)
    reasons = []
    manifest = {"version": VERSION, "schema_version": 1, "catalog_version": "coverage-v1", "config_version": "shadow-v1", "source_version": request.get("source_version", "unknown"), "scope": {"since_ms": since, "until_ms": until, "timezone": "UTC", "display_timezone": zone(request.get("display_timezone", "unknown")), "tasks": request["tasks"]}, "sampling": "explicit_inventory", "atomic_snapshot": False, "native_coverage": "not-started", "legacy": [], "database": {}, "limits": {"bytes": MAX_BYTES, "rows": MAX_ROWS, "input_bytes": 32 * 1024 * 1024, "seconds": MAX_SECONDS}, "supplements": {"of": request.get("supplement_of"), "grace_ms": request.get("grace_ms", 0)}}
    for key in ("source_version", "capture_config", "health"):
        if key not in request:
            reasons.append("unknown_" + key)
    manifest["capture_config"] = request.get("capture_config", "unknown")
    if isinstance(request.get("capture_config"), dict) and request["capture_config"].get("legacy_dropped", 0):
        reasons.append("legacy_sink_dropped")
    manifest["health"] = request.get("health", "unknown")
    health = request.get("health", {})
    if not isinstance(health, dict) or not health.get("runs"):
        reasons.append("unknown_capture_health")
    else:
        for h in health["runs"]:
            if any(h.get("dropped", [1])) or h.get("storage_failures", 1) or h.get("shutdown_timed_out", True):
                reasons.append("capture_loss_or_failure")
            if h.get("queue_depth", 1) or h.get("in_flight", 1) or h.get("accepted") != h.get("delivered"):
                reasons.append("capture_not_drained")
    if any(t.get("state") != "finished" for t in request["tasks"]):
        reasons.append("pending_tasks")
    # Only allowlisted typed fields from selected business rows; no arbitrary SQL from input.
    business = request.get("business")
    if business:
        try:
            if len(business.get("selections", [])) > 20:
                raise Incomplete("business_selection_limit")
            with closing(readonly(business["path"], budget)) as conn:
                allowed = {"upload_session": {"id", "streamer_info_id", "status", "submit_state", "submit_attempts", "blocked_count"}, "upload_missing_segment": {"id", "streamer_info_id", "upload_session_id", "status", "attempts", "attempt_phase", "uploaded_bytes"}, "upload_attempt": {"id", "missing_id", "phase_reached", "outcome", "line_source", "uploaded_bytes"}}
                for selection in business.get("selections", []):
                    table, columns, ids = selection["table"], selection["columns"], selection["ids"]
                    if table not in allowed or not set(columns) <= allowed[table] or not 0 < len(ids) <= 200 or any(type(i) is not int for i in ids):
                        raise Incomplete("invalid_business_selection")
                    rows = query(conn, 'SELECT ' + ','.join(columns) + ' FROM ' + table + ' WHERE id IN (' + ','.join('?' for _ in ids) + ') LIMIT 200', ids, budget)
                    for row in rows:
                        fields = dict(zip(columns, row))
                        if "id" in fields:
                            fields[{"upload_session":"upload_session_id", "upload_missing_segment":"missing_id", "upload_attempt":"history_row_id"}[table]] = fields.pop("id")
                        anon.seed(fields)
                        bundle.write("business.jsonl", {"ref": "business:" + str(bundle.files.get("business.jsonl", 0) + 1), "table": table, "fields": fields, "proves": "snapshot_only"})
                    if len(rows) != len(set(ids)):
                        reasons.append("business_rows_missing")
        except (sqlite3.Error, Incomplete, KeyError, ValueError):
            reasons.append("business_snapshot_incomplete")
    else:
        manifest["business"] = "unknown; no snapshot or durable history supplied"
    conn = None
    try:
        grace_ms = request.get("grace_ms", 0)
        if type(grace_ms) is not int or not 0 <= grace_ms <= 2000:
            raise Incomplete("invalid_grace_limit")
        if grace_ms:
            time.sleep(grace_ms / 1000)
        manifest["supplements"]["actual_grace_ms"] = grace_ms
        conn = readonly(request["database"], budget)
        application_id = query(conn, "PRAGMA application_id", (), budget)[0][0]
        if application_id != 0x424f4253:
            raise Incomplete("not_observability_database")
        high = query(conn, "SELECT COALESCE(MAX(id),0) FROM log_event", (), budget)[0][0]
        after = request.get("after_id", 0)
        if type(after) is not int or after < 0 or after > high:
            raise Incomplete("invalid_start_cursor")
        pruned, unclean, dirty = query(conn, "SELECT pruned_through,unclean_shutdowns,dirty FROM log_meta WHERE singleton=1", (), budget)[0]
        manifest["database"] = {"after_id": after, "high_water": high, "pruned_through": pruned, "unclean_shutdowns": unclean, "writer_dirty": dirty, "timezone": "UTC", "read_started_ms": time.time_ns() // 1000000}
        if pruned > after:
            reasons.append("retention_gap")
        if unclean:
            reasons.append("unclean_shutdown_window_unknown")
        # Seed the complete bounded set before formatting free text, so aliases are order independent.
        seed_cursor = after
        seed_count = 0
        processes = set()
        while seed_cursor < high:
            seeds = query(conn, "SELECT id,payload FROM log_event WHERE id>? AND id<=? AND occurred_at_ms>=? AND occurred_at_ms<=? ORDER BY id LIMIT 200", (seed_cursor, high, since, until), budget)
            if not seeds:
                break
            for seed_cursor, payload in seeds:
                seed_count += 1
                if seed_count > MAX_ROWS:
                    raise Incomplete("export_row_limit")
                budget.read_input(len(payload.encode()))
                seeded = json.loads(payload)
                processes.add((seeded.get("instance_id"), seeded.get("process_run_id")))
                anon.seed(seeded)
        health_processes = {(h.get("instance_id"), h.get("process_run_id")) for h in health.get("runs", [])} if isinstance(health, dict) else set()
        if not processes <= health_processes:
            reasons.append("process_health_missing")
        cursor = after
        while cursor < high:
            rows = query(conn, "SELECT id,ingested_at_ms,payload FROM log_event WHERE id>? AND id<=? AND occurred_at_ms>=? AND occurred_at_ms<=? ORDER BY id LIMIT 200", (cursor, high, since, until), budget)
            if not rows:
                break
            for row_id, ingested, payload in rows:
                if budget.rows >= MAX_ROWS:
                    raise Incomplete("export_row_limit")
                budget.rows += 1
                budget.read_input(len(payload.encode()))
                event = json.loads(payload)
                anon.seed(event)
                native = event.get("capture_kind") == "native"
                name = "native.jsonl" if native else "bridge.jsonl"
                diagnostics = query(conn, "SELECT payload FROM log_diagnostic WHERE event_uid=? LIMIT 1", (event["event_uid"],), budget)
                record = {"ref": "event:" + str(row_id), "id": row_id, "ingested_at_ms": ingested, "event": event, "diagnostic": json.loads(diagnostics[0][0]) if diagnostics else None}
                if not diagnostics and event["event_uid"] in request.get("required_diagnostics", []):
                    reasons.append("diagnostic_expired_or_missing")
                bundle.write(name, record)
                cursor = row_id
        manifest["database"]["last_exported_id"] = cursor
        manifest["database"]["read_finished_ms"] = time.time_ns() // 1000000
        end_high = query(conn, "SELECT COALESCE(MAX(id),0) FROM log_event", (), budget)[0][0]
        manifest["supplements"]["next_after_id"] = high
        manifest["supplements"]["observed_end_high_water"] = end_high
        if end_high > high:
            reasons.append("late_commits_require_supplement")
        if dirty and not request.get("grace_ms", 0):
            reasons.append("live_writer_grace_unknown")
        if query(conn, "SELECT pruned_through FROM log_meta WHERE singleton=1", (), budget)[0][0] > pruned:
            reasons.append("retention_changed_during_export")
    except (sqlite3.Error, ValueError, KeyError, Incomplete) as error:
        reasons.append(str(error) if isinstance(error, Incomplete) else "database_read_failed")
    finally:
        if conn:
            conn.close()
    # Freeze each old file independently. Appends after its fixed bound are a supplement, not read.
    if not request.get("legacy"):
        reasons.append("legacy_source_unavailable")
    for index, source in enumerate(request.get("legacy", [])):
        ref = "legacy-file:" + str(index + 1)
        entry = {"ref": ref, "timezone": zone(source.get("timezone", "unknown")), "complete": False}
        manifest["legacy"].append(entry)
        try:
            path = Path(source["path"])
            with os.fdopen(os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW), "rb") as f:
                stat = os.fstat(f.fileno())
                if not __import__('stat').S_ISREG(stat.st_mode):
                    raise Incomplete("legacy_not_regular_file")
                initial = generation(stat)
                start, end = source.get("start", 0), source.get("end", stat.st_size)
                if type(start) is not int or type(end) is not int or not 0 <= start <= end <= stat.st_size:
                    raise Incomplete("legacy_invalid_range")
                budget.read_input(end - start)
                entry.update({"generation": initial, "start": start, "end": end})
                if source.get("generation") and source["generation"] != initial:
                    raise Incomplete("legacy_generation_changed")
                if start:
                    f.seek(start - 1)
                    if f.read(1) != b"\n":
                        reasons.append("legacy_partial_first_line")
                f.seek(start)
                raw_hash = hashlib.sha256()
                while f.tell() < end:
                    budget.check()
                    offset = f.tell()
                    raw = f.readline(min(8193, end - offset))
                    if not raw:
                        raise Incomplete("legacy_truncated")
                    raw_hash.update(raw)
                    oversized = not raw.endswith(b"\n") and f.tell() < end
                    if oversized:
                        while not raw.endswith(b"\n") and f.tell() < end:
                            budget.check()
                            raw = f.readline(min(8193, end - f.tell()))
                            raw_hash.update(raw)
                        text = "[OMITTED:oversize]"
                        reasons.append("legacy_line_limit")
                    else:
                        text = raw.decode("utf-8", errors="strict").rstrip("\n")
                    if not raw.endswith(b"\n"):
                        reasons.append("legacy_partial_last_line")
                    bundle.write("legacy.jsonl", {"ref": ref + ":" + str(offset) + "-" + str(f.tell()), "file_ref": ref, "start": offset, "end": f.tell(), "text": text})
                entry["raw_sha256"] = raw_hash.hexdigest()
                current = generation(os.fstat(f.fileno()))
                named = generation(path.stat())
                if current != initial or named != initial:
                    raise Incomplete("legacy_changed_during_read")
                entry["complete"] = True
                if entry["timezone"] == "unknown":
                    reasons.append("legacy_timezone_unknown")
        except (OSError, UnicodeError, Incomplete, KeyError) as error:
            reasons.append(str(error) if isinstance(error, Incomplete) else "legacy_read_failed")
    if not bundle.files.get("legacy.jsonl"):
        reasons.append("no_legacy_records")
    if not bundle.files.get("native.jsonl") and not bundle.files.get("bridge.jsonl"):
        reasons.append("no_new_records")
    manifest["native_coverage"] = "requires-fact-validation" if bundle.files.get("native.jsonl") else "not-started"
    manifest["completeness"] = {"status": "insufficient" if reasons else "complete", "reasons": sorted(set(reasons))}
    manifest["metrics"] = {"output_bytes": budget.bytes, "event_rows": budget.rows, "query_max_ms": max(budget.query_ms, default=0), "elapsed_ms": (time.monotonic() - (budget.deadline - MAX_SECONDS)) * 1000}
    manifest["files"] = bundle.hashes()
    (out / "manifest.json").write_bytes(encoded(anon.clean(manifest)))
    # Only a separate private map can reconnect aliases to raw entities. Never an Agent input.
    (out / ".private-map.json").write_bytes(encoded([{"kind": k, "raw": raw, "alias": alias} for (k, raw), alias in anon.mapping.items()]))
    os.chmod(out / ".private-map.json", 0o600)
    return read_json(out / "manifest.json")


def validate(out, expectations=None):
    out = Path(out)
    manifest = read_json(out / "manifest.json")
    errors = []
    records, references, identities, sequences = [], set(), set(), set()
    for name, meta in manifest["files"].items():
        if name not in {"legacy.jsonl", "native.jsonl", "bridge.jsonl", "business.jsonl"}:
            errors.append({"ref": "manifest", "code": "invalid_file_reference"})
            continue
        with (out / name).open("rb") as f:
            data = f.read(MAX_BYTES + 1)
        if len(data) > MAX_BYTES or hashlib.sha256(data).hexdigest() != meta["sha256"]:
            errors.append({"ref": name, "code": "checksum_mismatch"})
        for raw in data.splitlines():
            r = json.loads(raw)
            ref = r["ref"]
            if ref in references:
                errors.append({"ref": ref, "code": "duplicate_reference"})
            references.add(ref)
            if "event" not in r:
                continue
            records.append(r)
            e = r["event"]
            identity = e.get("event_uid")
            sequence = (e.get("process_run_id"), e.get("sequence"))
            if identity in identities or sequence in sequences:
                errors.append({"ref": ref, "code": "duplicate_event_identity"})
            identities.add(identity)
            sequences.add(sequence)
            if e.get("schema_version") != 1 or e.get("level") not in {"TRACE", "DEBUG", "INFO", "WARN", "ERROR"} or e.get("category") not in CATEGORIES or not e.get("event_name", "").startswith(e.get("category", "") + "."):
                errors.append({"ref": ref, "code": "contract_header"})
            if (name == "native.jsonl") != (e.get("capture_kind") == "native"):
                errors.append({"ref": ref, "code": "capture_kind_mismatch"})
            if e.get("capture_kind") == "legacy_bridge" and e.get("event_name") != "system.legacy":
                errors.append({"ref": ref, "code": "bridge_claims_native_semantics"})
            fields = e.get("fields", {}).get("values", {})
            if fields.get("outcome", "unknown") not in OUTCOMES:
                errors.append({"ref": ref, "code": "invalid_outcome"})
            for key, value in fields.items():
                if key.endswith(("_ms", "_secs", "_bytes")) and (type(value) is not int or value < 0):
                    errors.append({"ref": ref, "code": "invalid_unit:" + key})
            if e.get("capture_kind") == "native":
                terminal = {"upload.failed":"failed", "upload.completed":"succeeded"}.get(e["event_name"])
                if terminal and fields.get("outcome") != terminal:
                    errors.append({"ref": ref, "code": "terminal_outcome_conflict"})
                for ambiguous in ("duration", "size", "delay", "gap", "previous", "current"):
                    if ambiguous in fields:
                        errors.append({"ref": ref, "code": "unit_missing:" + ambiguous})
                for key in REQUIRED.get(e["event_name"], []):
                    if key not in fields:
                        errors.append({"ref": ref, "code": "missing_field:" + key})
    # Stable identities cannot change owners across stages. Missing owners stay unknown.
    owners = {}
    for record in records:
        event = record["event"]
        if event.get("capture_kind") != "native":
            continue
        fields = event.get("fields", {}).get("values", {})
        for key, owner_fields in [("segment_id", ("task_id", "streamer_info_id", "original_file")), ("upload_attempt_id", ("segment_id", "upload_session_id")), ("download_attempt_id", ("task_id", "streamer_info_id"))]:
            if key not in fields:
                continue
            identity = (event.get("instance_id"), key, fields[key])
            known = owners.setdefault(identity, {})
            for owner in owner_fields:
                if owner in fields:
                    if owner in known and known[owner][0] != fields[owner]:
                        errors.append({"ref": record["ref"], "related_ref": known[owner][1], "code": "association_conflict:" + owner})
                    else:
                        known[owner] = (fields[owner], record["ref"])
    # Inventory precedes logs; facts present in neither source are still in the denominator.
    facts = []
    for expected in expectations or []:
        matches = [r for r in records if r["event"]["capture_kind"] == "native" and r["event"]["event_name"] == expected["event_name"] and all(r["event"]["fields"]["values"].get(k) == v for k, v in expected.get("fields", {}).items())]
        facts.append({"fact_id": expected["fact_id"], "status": "confirmed" if matches else "unknown", "refs": [r["ref"] for r in matches]})
        if not matches:
            errors.append({"ref": "scenario:" + expected["fact_id"], "code": "expected_fact_missing_or_conflicting"})
    # Incomplete sources never pass, even when a difference also exists in the captured subset.
    status = "insufficient" if manifest["completeness"]["status"] != "complete" else "failed" if errors else "passed"
    return {"version": VERSION, "status": status, "errors": errors, "facts": facts, "native_coverage": manifest["native_coverage"], "scope": "transport_and_contract_only" if not expectations else "explicit_expected_facts", "references": sorted(references)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("export")
    run.add_argument("request", type=Path)
    run.add_argument("output", type=Path)
    check = sub.add_parser("validate")
    check.add_argument("bundle", type=Path)
    check.add_argument("--expectations", type=Path)
    args = parser.parse_args()
    if args.command == "export":
        result = export(read_json(args.request), args.output)
        print(json.dumps(result["completeness"]))
        return 0 if result["completeness"]["status"] == "complete" else 2
    result = validate(args.bundle, read_json(args.expectations) if args.expectations else None)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["status"] == "passed" else 2


if __name__ == "__main__":
    raise SystemExit(main())
