#!/usr/bin/env python3
"""Verify that every Rust source file with legacy tracing macros has a reviewed disposition."""

from __future__ import annotations

import json
import re
import sys
from collections import Counter
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CATALOG = ROOT / ".scratch/structured-logging/diagnostic-classification-v1.json"
LOG_MACRO = re.compile(r"\b(?:trace|debug|info|warn|error)!\s*\(")
ALLOWED = {
    "native_covered",
    "retain_bridge",
    "no_persistence",
    "explicitly_unsupported",
    "coverage_gap",
}


def fail(message: str) -> None:
    print(f"diagnostic classification invalid: {message}", file=sys.stderr)
    raise SystemExit(1)


def scanned_files(roots: list[str]) -> tuple[set[str], int]:
    files: set[str] = set()
    callsites = 0
    for root_name in roots:
        source_root = ROOT / root_name
        if not source_root.is_dir():
            fail(f"scan root does not exist: {root_name}")
        for path in source_root.rglob("*.rs"):
            if "tests" in path.parts or "examples" in path.parts:
                continue
            text = path.read_text(encoding="utf-8")
            matches = LOG_MACRO.findall(text)
            if matches:
                files.add(path.relative_to(ROOT).as_posix())
                callsites += len(matches)
    return files, callsites


def main() -> None:
    data = json.loads(CATALOG.read_text(encoding="utf-8"))
    if data.get("version") != "diagnostic-classification-v1":
        fail("unexpected catalog version")
    roots = data.get("scan_roots")
    if not isinstance(roots, list) or not roots:
        fail("scan_roots must be a non-empty list")

    catalog_files: set[str] = set()
    decisions: Counter[str] = Counter()
    for group in data.get("groups", []):
        decision = group.get("decision")
        reason = group.get("reason")
        paths = group.get("paths")
        if decision not in ALLOWED:
            fail(f"unknown decision: {decision!r}")
        if not isinstance(reason, str) or not reason.strip():
            fail(f"missing reason for {decision}")
        if not isinstance(paths, list):
            fail(f"{decision} must list paths")
        if not paths and decision != "coverage_gap":
            fail(f"{decision} must list at least one path")
        for relative in paths:
            if relative in catalog_files:
                fail(f"duplicate path: {relative}")
            path = ROOT / relative
            if not path.is_file():
                fail(f"catalog path does not exist: {relative}")
            catalog_files.add(relative)
            decisions[decision] += 1

    boundaries = data.get("explicitly_unsupported_boundaries")
    if not isinstance(boundaries, list) or not boundaries:
        fail("explicitly_unsupported_boundaries must not be empty")
    for item in boundaries:
        if not isinstance(item.get("boundary"), str) or not item["boundary"].strip():
            fail("unsupported boundary needs a name")
        if not isinstance(item.get("reason"), str) or not item["reason"].strip():
            fail(f"unsupported boundary needs a reason: {item.get('boundary')!r}")

    observed, callsites = scanned_files(roots)
    missing = sorted(observed - catalog_files)
    stale = sorted(catalog_files - observed)
    if missing:
        fail("unclassified logging files: " + ", ".join(missing))
    if stale:
        fail("catalog paths no longer contain logging macros: " + ", ".join(stale))

    summary = ", ".join(f"{key}={decisions[key]}" for key in sorted(decisions))
    print(
        f"diagnostic classification is current: {len(observed)} files, "
        f"{callsites} conservative macro sites; {summary}"
    )


if __name__ == "__main__":
    main()
