#!/usr/bin/env python3
"""Validate the hand-maintained CODE_INDEX.md navigation structure."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ENTRY_RE = re.compile(r"^\| `([^`]+)` \|")
RELATION_RE = re.compile(r"^- `([^`]+)` → `([^`]+)`")


def validate_path(repo_root: Path, raw_path: str, context: str) -> list[str]:
    errors: list[str] = []
    path = Path(raw_path)

    if path.is_absolute():
        return [f"{context}: path must be relative to the repository: {raw_path}"]

    resolved = (repo_root / path).resolve()
    if not resolved.is_relative_to(repo_root):
        errors.append(f"{context}: path escapes the repository: {raw_path}")
    elif not resolved.is_file():
        errors.append(f"{context}: file does not exist: {raw_path}")

    return errors


def main() -> int:
    repo_root = Path(__file__).resolve().parents[1]
    index_path = repo_root / "CODE_INDEX.md"
    lines = index_path.read_text(encoding="utf-8").splitlines()

    entries: dict[str, int] = {}
    relations: set[tuple[str, str]] = set()
    relation_rows: list[tuple[int, str, str]] = []
    errors: list[str] = []

    for line_number, line in enumerate(lines, start=1):
        if match := ENTRY_RE.match(line):
            raw_path = match.group(1)
            if previous := entries.get(raw_path):
                errors.append(
                    f"line {line_number}: duplicate file entry {raw_path} "
                    f"(first listed on line {previous})"
                )
            else:
                entries[raw_path] = line_number
            errors.extend(validate_path(repo_root, raw_path, f"line {line_number}"))
            continue

        if match := RELATION_RE.match(line):
            source, target = match.groups()
            edge = (source, target)
            if edge in relations:
                errors.append(
                    f"line {line_number}: duplicate relationship {source} -> {target}"
                )
            relations.add(edge)
            relation_rows.append((line_number, source, target))

    if not entries:
        errors.append("CODE_INDEX.md contains no file entries")

    for line_number, source, target in relation_rows:
        for endpoint in (source, target):
            errors.extend(validate_path(repo_root, endpoint, f"line {line_number}"))
            if endpoint not in entries:
                errors.append(
                    f"line {line_number}: relationship endpoint has no file entry: {endpoint}"
                )

    if errors:
        print("CODE_INDEX.md validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    print(
        f"CODE_INDEX.md is valid: {len(entries)} files, "
        f"{len(relations)} relationships"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
