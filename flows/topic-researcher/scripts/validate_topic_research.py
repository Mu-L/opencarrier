#!/usr/bin/env python3
"""Validate a writing-pipeline 素材.md — the research notes emitted by
topic-researcher (shape only; research quality is the system prompt + human
review's job). TOPIC_OK / TOPIC_INVALID:N + return 0/1.

Light gate (topic-researcher is the chain entry, mostly free-form research):
non-empty, carries a well-formed pipeline id line, and has enough breadth
(≥3 distinct research items / sources). Pure structure.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

PIPELINE_LINE_RE = re.compile(r"流水线ID:\s*(pipeline-\d{8}-\S+)")
ITEM_RE = re.compile(r"^\s*(?:[-*+]\s+|\d+[.、)]\s+|https?://)")


def cjk_len(s: str) -> int:
    return sum(1 for c in s if not c.isspace())


def validate(text: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    warnings: list[str] = []

    if not text.strip():
        errors.append("素材.md is empty")
        return errors, warnings

    if not PIPELINE_LINE_RE.search(text):
        errors.append("missing `流水线ID: pipeline-YYYYMMDD-<topic>` line")

    items = [ln for ln in text.splitlines() if ITEM_RE.match(ln)]
    if len(items) < 3:
        errors.append(f"needs ≥3 research items (list entries or URLs); got {len(items)}")

    if cjk_len(text) < 200:
        warnings.append("素材 is <200 chars — research may be too thin to support an outline")

    return errors, warnings


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable result")
    args = parser.parse_args()
    try:
        text = args.file.expanduser().read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"Cannot read 素材: {exc}") from exc
    errors, warnings = validate(text)
    result = {"ok": not errors, "errors": errors, "warnings": warnings}
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    else:
        for w in warnings:
            print(f"WARNING:{w}")
        for e in errors:
            print(f"ERROR:{e}")
        print("TOPIC_OK" if not errors else f"TOPIC_INVALID:{len(errors)}")
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main())
