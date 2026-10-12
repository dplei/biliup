#!/usr/bin/env python3
"""Validate the root navigation and docs/code-index/*.md together."""

from __future__ import annotations

import re
import sys
from pathlib import Path


ENTRY_RE = re.compile(r"^\| `([^`]+)` \|")
LINK_RE = re.compile(r"\[[^\]]+\]\(([^)]+\.md)\)")


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


def validate_index(repo_root: Path) -> tuple[list[str], int, int]:
    repo_root = repo_root.resolve()
    index_paths = [
        repo_root / "CODE_INDEX.md",
        *sorted((repo_root / "docs/code-index").glob("*.md")),
    ]
    entries: dict[str, str] = {}
    relations: set[str] = set()
    relation_rows: list[tuple[str, tuple[str, ...]]] = []
    root_links: set[Path] = set()
    errors: list[str] = []

    for index_path in index_paths:
        relative = index_path.relative_to(repo_root)
        path_errors = validate_path(repo_root, str(relative), str(relative))
        if path_errors:
            errors.extend(path_errors)
            continue
        lines = index_path.read_text(encoding="utf-8").splitlines()
        for line_number, line in enumerate(lines, start=1):
            context = f"{relative}:{line_number}"
            for link in LINK_RE.findall(line):
                if "://" in link:
                    continue
                target = index_path.parent / link
                if index_path == index_paths[0]:
                    root_links.add(target.resolve())
                raw_path = str(relative.parent / link)
                errors.extend(validate_path(repo_root, raw_path, context))

            if match := ENTRY_RE.match(line):
                raw_path = match.group(1)
                if previous := entries.get(raw_path):
                    errors.append(
                        f"{context}: duplicate file entry {raw_path} "
                        f"(first listed at {previous})"
                    )
                else:
                    entries[raw_path] = context
                errors.extend(validate_path(repo_root, raw_path, context))
                continue

            if line.startswith("- `") and " → " in line:
                # Only the topology before the explanation contains file endpoints.
                topology = re.split("[（：]", line, maxsplit=1)[0].strip()
                endpoints = tuple(re.findall(r"`([^`]+)`", topology))
                if topology in relations:
                    errors.append(f"{context}: duplicate relationship {topology[2:]}")
                relations.add(topology)
                relation_rows.append((context, endpoints))

    for index_path in index_paths[1:]:
        if index_path.resolve() not in root_links:
            errors.append(f"{index_path.relative_to(repo_root)}: missing link from CODE_INDEX.md")

    if not entries:
        errors.append("code index contains no file entries")

    for context, endpoints in relation_rows:
        for endpoint in endpoints:
            errors.extend(validate_path(repo_root, endpoint, context))
            if endpoint not in entries:
                errors.append(f"{context}: relationship endpoint has no file entry: {endpoint}")

    return errors, len(entries), len(relations)


def main() -> int:
    errors, entry_count, relation_count = validate_index(Path(__file__).resolve().parents[1])
    if errors:
        print("CODE_INDEX.md validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    print(
        f"CODE_INDEX.md is valid: {entry_count} files, "
        f"{relation_count} relationships"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
