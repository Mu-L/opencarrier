#!/usr/bin/env python3
"""Validate a writing-pipeline 素材.md — the research notes emitted by
topic-researcher (shape only; research quality is the system prompt + human
review's job). TOPIC_OK / TOPIC_INVALID:N + return 0/1.

Light gate (topic-researcher is the chain entry, mostly free-form research):
non-empty, carries a well-formed pipeline id line, and has enough breadth
(>=3 distinct research items / sources). Pure structure.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Shared utility (validate_util.py at ~/.opencarrier/flows/).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import validate_util  # noqa: E402

PIPELINE_LINE_RE = re.compile(r"流水线ID:\s*(pipeline-\d{8}-\S+)")
ITEM_RE = re.compile(r"^\s*(?:[-*+]\s+|\d+[.、)]\s+|https?://)")


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
        errors.append(f"needs >=3 research items (list entries or URLs); got {len(items)}")

    if validate_util.cjk_len(text) < 200:
        warnings.append("素材 is <200 chars — research may be too thin to support an outline")

    return errors, warnings


if __name__ == "__main__":
    sys.exit(validate_util.run_main(validate, "TOPIC", __doc__))
