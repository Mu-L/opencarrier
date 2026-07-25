#!/usr/bin/env python3
"""Shared utilities for flow validator scripts.

Each validator implements a `validate(text) -> (errors, warnings)` function
and delegates CLI boilerplate to `run_main()`. Example::

    def validate(text: str) -> tuple[list[str], list[str]]:
        errors, warnings = [], []
        # ... unique validation logic ...
        return errors, warnings

    if __name__ == "__main__":
        import validate_util
        sys.exit(validate_util.run_main(validate, "MYFLOW"))
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


META_RE = re.compile(r"^META_(\w+):\s*(.*)$")


def parse_meta_head(text: str) -> tuple[dict[str, str], str]:
    """Split a leading ``<!-- ... -->`` META block.

    Returns ``(meta, body)`` where *meta* maps lowercased keys
    (``title`` / ``author`` / …) to values and *body* is the text after the
    closing ``-->``.  When there is no META block, returns ``({}, text)``.
    """
    s = text.lstrip()
    if not s.startswith("<!--"):
        return {}, text
    end = s.find("-->")
    if end == -1:
        return {}, text
    block = s[3:end]
    body = s[end + 3 :].lstrip("\n")
    meta: dict[str, str] = {}
    for line in block.splitlines():
        m = META_RE.match(line.strip())
        if m:
            meta[m.group(1).lower()] = m.group(2).strip()
    return meta, body


def cjk_len(s: str) -> int:
    """Count characters excluding whitespace."""
    return sum(1 for c in s if not c.isspace())


def run_main(
    validate_fn: callable,
    ok_marker: str,
    description: str = "",
) -> int:
    """Run a validator from ``__main__``.

    *validate_fn* receives the file content and returns
    ``(errors, warnings)``.

    *ok_marker* is the third segment of the OK/INVALID status line,
    e.g. ``"ARTICLE"`` produces ``ARTICLE_OK`` / ``ARTICLE_INVALID:N``.

    Returns ``0`` on success, ``1`` on failure (exit code).
    """
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("file", type=Path)
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON")
    args = parser.parse_args()
    try:
        text = args.file.expanduser().read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"Cannot read {args.file}: {exc}") from exc
    errors, warnings = validate_fn(text)
    result = {"ok": not errors, "errors": errors, "warnings": warnings}
    if args.json:
        print(json.dumps(result, ensure_ascii=False, indent=2))
    else:
        for w in warnings:
            print(f"WARNING:{w}")
        for e in errors:
            print(f"ERROR:{e}")
        print(f"{ok_marker}_OK" if not errors else f"{ok_marker}_INVALID:{len(errors)}")
    return 0 if not errors else 1
